use std::fs::{self, Metadata};
use std::io::{Error as IoError, ErrorKind as IoErrorKind, Write};
use std::path::Path;
use std::sync::Arc;

use salvo::prelude::*;
use xml::common::{Position, XmlVersion};
use xml::name::{Name as XmlName, OwnedName as OwnedXmlName};
use xml::reader::{EventReader as XmlReader, XmlEvent as XmlREvent};
use xml::writer::{EventWriter as XmlWriter, XmlEvent as XmlWEvent};
use xml::{EmitterConfig as XmlEmitterConfig, ParserConfig as XmlParserConfig};

use crate::config::{AppConfig, log_msg};
use crate::util::webdav::*;
use crate::util::write_conditions::{MUTATION_LOCK, WriteConditions};
use crate::util::*;

fn default_xml_parser_config() -> XmlParserConfig {
    let mut config = XmlParserConfig::new();
    config.trim_whitespace = true;
    config.whitespace_to_characters = true;
    config
}

fn default_xml_emitter_config() -> XmlEmitterConfig {
    let mut config = XmlEmitterConfig::new();
    config.perform_indent = cfg!(debug_assertions);
    config
}

const CONTROL_BODY_LIMIT: usize = 64 * 1024;
const HREF_SEGMENT: &percent_encoding::AsciiSet = &percent_encoding::NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

async fn control_body(req: &mut Request, res: &mut Response) -> Option<Vec<u8>> {
    control_body_with_timeout(req, res, std::time::Duration::from_secs(30)).await
}

async fn control_body_with_timeout(
    req: &mut Request,
    res: &mut Response,
    timeout: std::time::Duration,
) -> Option<Vec<u8>> {
    let status =
        match tokio::time::timeout(timeout, req.payload_with_max_size(CONTROL_BODY_LIMIT)).await {
            Ok(Ok(bytes)) if bytes.len() <= CONTROL_BODY_LIMIT => return Some(bytes.to_vec()),
            Ok(Ok(_)) | Ok(Err(salvo::http::ParseError::PayloadTooLarge)) => {
                StatusCode::PAYLOAD_TOO_LARGE
            }
            Ok(Err(_)) => StatusCode::BAD_REQUEST,
            Err(_) => StatusCode::REQUEST_TIMEOUT,
        };
    res.status_code(status);
    res.render(Text::Plain("Unable to read the WebDAV request body."));
    None
}

// ─── PROPFIND ──────────────────────────────────────────

#[handler]
pub async fn handle_propfind(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let config = depot.get_typed::<Arc<AppConfig>>().unwrap().clone();

    if let Some(resp) = crate::hoops::auth::check_auth(req, &config) {
        *res = resp;
        return;
    }

    let url_path_raw = req.uri().path().to_string();
    let segments: Vec<&str> = url_path_raw.split('/').filter(|s| !s.is_empty()).collect();
    let (req_p, symlink, url_err) = resolve_path(
        &config.hosted_directory.1,
        &segments,
        config.follow_symlinks,
    );

    if url_err {
        set_error(
            res,
            StatusCode::BAD_REQUEST,
            "400 Bad Request",
            "Percent-encoding decoded to invalid UTF-8.",
        );
        return;
    }

    if !req_p.exists() || config.is_symlink_denied(symlink, &req_p) {
        set_error(
            res,
            StatusCode::NOT_FOUND,
            "404 Not Found",
            "The requested entity doesn't exist.",
        );
        return;
    }

    // Infinite-depth traversal is deliberately unsupported. A missing Depth
    // header means infinity in WebDAV and receives the same finite-depth error.
    let depth = match req
        .headers()
        .get("Depth")
        .and_then(|value| value.to_str().ok())
    {
        Some("0") => Depth::Zero,
        Some("1") => Depth::One,
        None | Some("infinity") => {
            res.status_code(StatusCode::FORBIDDEN);
            res.headers_mut().insert(
                salvo::http::header::CONTENT_TYPE,
                "application/xml".parse().unwrap(),
            );
            res.write_body(r#"<?xml version="1.0" encoding="utf-8"?><D:error xmlns:D="DAV:"><D:propfind-finite-depth/></D:error>"#).ok();
            return;
        }
        _ => {
            set_error(
                res,
                StatusCode::BAD_REQUEST,
                "400 Bad Request",
                "Depth must be 0 or 1.",
            );
            return;
        }
    };

    if depth != Depth::Zero
        && let Some(response) =
            crate::hoops::auth::check_tree_auth_async(req, &config, &url_path_raw).await
    {
        *res = response;
        return;
    }

    let Some(body_bytes) = control_body(req, res).await else {
        return;
    };
    let props = match parse_propfind(&body_bytes) {
        Ok(p) => p,
        Err(e) => match e {
            PropfindParseError::EmptyBody => PropfindVariant::AllProp,
            PropfindParseError::XmlError(msg) => {
                let remote = req.remote_addr().to_string();
                log_msg(
                    config.log,
                    &format!(
                        "{} tried to PROPFIND {} with invalid XML",
                        remote,
                        req_p.display()
                    ),
                );
                set_error(
                    res,
                    StatusCode::BAD_REQUEST,
                    "400 Bad Request",
                    &format!("Invalid XML: {}", msg),
                );
                return;
            }
        },
    };

    let remote = req.remote_addr().to_string();
    log_msg(
        config.log,
        &format!(
            "{} requested PROPFIND of {} on {} at depth {}",
            remote,
            props,
            req_p.display(),
            depth
        ),
    );

    let url = req.uri().path().to_string();
    let ua = req
        .headers()
        .get(salvo::http::header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);

    let work_config = config.clone();
    let Some(job) = config.spawn_blocking(move || {
        write_propfind_output(&work_config, &url, &req_p, &props, ua.as_deref(), depth)
    }) else {
        res.status_code(StatusCode::SERVICE_UNAVAILABLE);
        return;
    };
    let result = match job.await {
        Ok(result) => result,
        Err(_) => {
            res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
            return;
        }
    };
    match result {
        Ok(xml_bytes) => {
            res.status_code(StatusCode::MULTI_STATUS);
            res.headers_mut().insert(
                salvo::http::header::CONTENT_TYPE,
                "text/xml; charset=utf-8".parse().unwrap(),
            );
            res.headers_mut()
                .insert(salvo::http::header::SERVER, USER_AGENT.parse().unwrap());
            res.write_body(xml_bytes).ok();
        }
        Err(e) => {
            let over_budget = matches!(&e, xml::writer::Error::Io(error) if error.kind() == IoErrorKind::FileTooLarge);
            set_error(
                res,
                if over_budget {
                    StatusCode::INSUFFICIENT_STORAGE
                } else {
                    StatusCode::INTERNAL_SERVER_ERROR
                },
                if over_budget {
                    "507 Insufficient Storage"
                } else {
                    "500 Internal Server Error"
                },
                &format!("XML error: {}", e),
            );
        }
    }
}

fn write_propfind_output(
    config: &AppConfig,
    url: &str,
    path: &Path,
    variant: &PropfindVariant,
    ua: Option<&str>,
    depth: Depth,
) -> Result<Vec<u8>, xml::writer::Error> {
    let props: &[&[XmlName]] = match variant {
        PropfindVariant::AllProp => {
            if client_microsoft(ua) {
                WEBDAV_ALLPROP_PROPERTIES_WINDOWS
            } else {
                WEBDAV_ALLPROP_PROPERTIES_NON_WINDOWS
            }
        }
        PropfindVariant::PropName => WEBDAV_PROPNAME_PROPERTIES,
        PropfindVariant::Props(_) => &[],
    };

    let just_names = matches!(variant, PropfindVariant::PropName);

    let mut out = initialise_xml_output_with(crate::util::metadata::MetadataBuffer::new(
        crate::util::metadata::MAX_METADATA_BYTES,
    ))?;

    // Build namespace-aware start element
    let mut start = XmlWEvent::start_element("D:multistatus")
        .ns(WEBDAV_XML_NAMESPACES[0].0, WEBDAV_XML_NAMESPACES[0].1);
    for &(prefix, namespace) in &WEBDAV_XML_NAMESPACES[1..] {
        start = start.ns(prefix, namespace);
    }
    out.write(start)?;

    let meta = path.metadata()?;

    if let PropfindVariant::Props(custom) = variant {
        write_propfind_response_custom(config, &mut out, url, path, &meta, custom, just_names)?;
    } else {
        write_propfind_response(config, &mut out, url, path, &meta, props, just_names)?;
    }

    if meta.is_dir() {
        let mut url_owned = url.to_string();
        if let PropfindVariant::Props(custom) = variant {
            write_propfind_recursive_custom(
                config,
                &mut out,
                &mut url_owned,
                path,
                custom,
                just_names,
                depth,
            )?;
        } else {
            write_propfind_recursive(
                config,
                &mut out,
                &mut url_owned,
                path,
                props,
                just_names,
                depth,
            )?;
        }
    }

    out.write(XmlWEvent::end_element())?;
    Ok(out.into_inner().into_bytes())
}

fn write_propfind_response<W: Write>(
    config: &AppConfig,
    out: &mut XmlWriter<W>,
    url: &str,
    path: &Path,
    meta: &Metadata,
    props: &[&[XmlName]],
    just_names: bool,
) -> Result<(), xml::writer::Error> {
    out.write(XmlWEvent::start_element("D:response"))?;

    out.write(XmlWEvent::start_element("D:href"))?;
    out.write(XmlWEvent::characters(url))?;
    out.write(XmlWEvent::end_element())?;

    let prop_count: usize = props.iter().map(|pp| pp.len()).sum();
    let mut failed_props = Vec::with_capacity(prop_count);

    out.write(XmlWEvent::start_element("D:propstat"))?;
    out.write(XmlWEvent::start_element("D:prop"))?;

    for prop in props.iter().flat_map(|pp| pp.iter()) {
        let mut write_name = false;
        if !just_names && !write_prop_value(config, out, path, meta, *prop)? {
            failed_props.push(*prop);
            write_name = true;
        }
        if just_names || write_name {
            write_start_prop_element(out, *prop)?;
            out.write(XmlWEvent::end_element())?;
        }
    }

    out.write(XmlWEvent::end_element())?; // prop

    out.write(XmlWEvent::start_element("D:status"))?;
    if failed_props.len() >= prop_count {
        out.write(XmlWEvent::characters("HTTP/1.1 404 Not Found"))?;
        out.write(XmlWEvent::end_element())?; // status
        out.write(XmlWEvent::end_element())?; // propstat
        out.write(XmlWEvent::end_element())?; // response
        return Ok(());
    }

    out.write(XmlWEvent::characters("HTTP/1.1 200 OK"))?;
    out.write(XmlWEvent::end_element())?; // status
    out.write(XmlWEvent::end_element())?; // propstat

    if !failed_props.is_empty() {
        out.write(XmlWEvent::start_element("D:propstat"))?;
        out.write(XmlWEvent::start_element("D:prop"))?;
        for prop in failed_props {
            write_start_prop_element(out, prop)?;
            out.write(XmlWEvent::end_element())?;
        }
        out.write(XmlWEvent::end_element())?; // prop
        out.write(XmlWEvent::start_element("D:status"))?;
        out.write(XmlWEvent::characters("HTTP/1.1 404 Not Found"))?;
        out.write(XmlWEvent::end_element())?; // status
        out.write(XmlWEvent::end_element())?; // propstat
    }

    out.write(XmlWEvent::end_element())?; // response
    Ok(())
}

fn write_propfind_response_custom<W: Write>(
    config: &AppConfig,
    out: &mut XmlWriter<W>,
    url: &str,
    path: &Path,
    meta: &Metadata,
    props: &[OwnedXmlName],
    just_names: bool,
) -> Result<(), xml::writer::Error> {
    out.write(XmlWEvent::start_element("D:response"))?;

    out.write(XmlWEvent::start_element("D:href"))?;
    out.write(XmlWEvent::characters(url))?;
    out.write(XmlWEvent::end_element())?;

    let mut failed_props = Vec::with_capacity(props.len());

    out.write(XmlWEvent::start_element("D:propstat"))?;
    out.write(XmlWEvent::start_element("D:prop"))?;

    for prop in props {
        let name = prop.borrow();
        let mut write_name = false;
        if !just_names && !write_prop_value(config, out, path, meta, name)? {
            failed_props.push(name);
            write_name = true;
        }
        if just_names || write_name {
            write_start_prop_element(out, name)?;
            out.write(XmlWEvent::end_element())?;
        }
    }

    out.write(XmlWEvent::end_element())?; // prop
    out.write(XmlWEvent::start_element("D:status"))?;

    if failed_props.len() >= props.len() {
        out.write(XmlWEvent::characters("HTTP/1.1 404 Not Found"))?;
        out.write(XmlWEvent::end_element())?; // status
        out.write(XmlWEvent::end_element())?; // propstat
        out.write(XmlWEvent::end_element())?; // response
        return Ok(());
    }

    out.write(XmlWEvent::characters("HTTP/1.1 200 OK"))?;
    out.write(XmlWEvent::end_element())?; // status
    out.write(XmlWEvent::end_element())?; // propstat

    if !failed_props.is_empty() {
        out.write(XmlWEvent::start_element("D:propstat"))?;
        out.write(XmlWEvent::start_element("D:prop"))?;
        for prop in failed_props {
            write_start_prop_element(out, prop)?;
            out.write(XmlWEvent::end_element())?;
        }
        out.write(XmlWEvent::end_element())?; // prop
        out.write(XmlWEvent::start_element("D:status"))?;
        out.write(XmlWEvent::characters("HTTP/1.1 404 Not Found"))?;
        out.write(XmlWEvent::end_element())?; // status
        out.write(XmlWEvent::end_element())?; // propstat
    }

    out.write(XmlWEvent::end_element())?; // response
    Ok(())
}

fn write_propfind_recursive<W: Write>(
    config: &AppConfig,
    out: &mut XmlWriter<W>,
    root_url: &mut String,
    root_path: &Path,
    props: &[&[XmlName]],
    just_names: bool,
    depth: Depth,
) -> Result<(), xml::writer::Error> {
    if !root_url.ends_with('/') {
        root_url.push('/');
    }
    let root_url_orig_len = root_url.len();
    let mut links_left = MAX_SYMLINKS;

    if let Some(next_depth) = depth.lower() {
        let entries = crate::util::metadata::directory_entries(root_path)?;
        for f in entries {
            if is_internal_name(&f.file_name()) {
                continue;
            }
            root_url.truncate(root_url_orig_len);
            root_url.push_str(
                &percent_encoding::utf8_percent_encode(
                    &f.file_name().to_string_lossy(),
                    HREF_SEGMENT,
                )
                .to_string(),
            );

            let mut path = f.path();
            let mut symlink = false;
            while let Ok(newlink) = path.read_link() {
                symlink = true;
                if links_left != 0 {
                    if newlink.is_absolute() {
                        path = newlink;
                    } else {
                        path.pop();
                        path.push(newlink);
                    }
                    links_left -= 1;
                } else {
                    break;
                }
            }

            if path.exists() && !config.is_symlink_denied(symlink, &path) {
                let metadata = match path.metadata() {
                    Ok(m) => m,
                    Err(_) => continue,
                };
                write_propfind_response(
                    config, out, root_url, &path, &metadata, props, just_names,
                )?;
                if metadata.is_dir() {
                    write_propfind_recursive(
                        config, out, root_url, &path, props, just_names, next_depth,
                    )?;
                }
            }
        }
    }
    Ok(())
}

fn write_propfind_recursive_custom<W: Write>(
    config: &AppConfig,
    out: &mut XmlWriter<W>,
    root_url: &mut String,
    root_path: &Path,
    props: &[OwnedXmlName],
    just_names: bool,
    depth: Depth,
) -> Result<(), xml::writer::Error> {
    if !root_url.ends_with('/') {
        root_url.push('/');
    }
    let root_url_orig_len = root_url.len();
    let mut links_left = MAX_SYMLINKS;

    if let Some(next_depth) = depth.lower() {
        let entries = crate::util::metadata::directory_entries(root_path)?;
        for f in entries {
            if is_internal_name(&f.file_name()) {
                continue;
            }
            root_url.truncate(root_url_orig_len);
            root_url.push_str(
                &percent_encoding::utf8_percent_encode(
                    &f.file_name().to_string_lossy(),
                    HREF_SEGMENT,
                )
                .to_string(),
            );

            let mut path = f.path();
            let mut symlink = false;
            while let Ok(newlink) = path.read_link() {
                symlink = true;
                if links_left != 0 {
                    if newlink.is_absolute() {
                        path = newlink;
                    } else {
                        path.pop();
                        path.push(newlink);
                    }
                    links_left -= 1;
                } else {
                    break;
                }
            }

            if path.exists() && !config.is_symlink_denied(symlink, &path) {
                let metadata = match path.metadata() {
                    Ok(m) => m,
                    Err(_) => continue,
                };
                write_propfind_response_custom(
                    config, out, root_url, &path, &metadata, props, just_names,
                )?;
                if metadata.is_dir() {
                    write_propfind_recursive_custom(
                        config, out, root_url, &path, props, just_names, next_depth,
                    )?;
                }
            }
        }
    }
    Ok(())
}

fn write_prop_value<W: Write>(
    config: &AppConfig,
    out: &mut XmlWriter<W>,
    path: &Path,
    meta: &Metadata,
    prop: XmlName,
) -> Result<bool, xml::writer::Error> {
    if prop.namespace == Some(WEBDAV_XML_NAMESPACE_DAV.1) {
        match prop.local_name {
            "creationdate" => {
                out.write(XmlWEvent::start_element((
                    WEBDAV_XML_NAMESPACE_DAV.0,
                    "creationdate",
                )))?;
                out.write(XmlWEvent::characters(&file_time_created(meta).to_rfc3339()))?;
            }
            "getcontentlength" => {
                out.write(XmlWEvent::start_element((
                    WEBDAV_XML_NAMESPACE_DAV.0,
                    "getcontentlength",
                )))?;
                out.write(XmlWEvent::characters(&file_length(meta, &path).to_string()))?;
            }
            "getcontenttype" => {
                out.write(XmlWEvent::start_element((
                    WEBDAV_XML_NAMESPACE_DAV.0,
                    "getcontenttype",
                )))?;
                out.write(XmlWEvent::characters(&guess_mime_type(
                    path,
                    &config.mime_type_overrides,
                )))?;
            }
            "getlastmodified" => {
                out.write(XmlWEvent::start_element((
                    WEBDAV_XML_NAMESPACE_DAV.0,
                    "getlastmodified",
                )))?;
                out.write(XmlWEvent::characters(
                    &file_time_modified(meta).to_rfc3339(),
                ))?;
            }
            "resourcetype" => {
                out.write(XmlWEvent::start_element((
                    WEBDAV_XML_NAMESPACE_DAV.0,
                    "resourcetype",
                )))?;
                if !is_actually_file(&meta.file_type(), path) {
                    out.write(XmlWEvent::start_element((
                        WEBDAV_XML_NAMESPACE_DAV.0,
                        "collection",
                    )))?;
                    out.write(XmlWEvent::end_element())?;
                }
            }
            _ => return Ok(false),
        }
    } else if prop.namespace == Some(WEBDAV_XML_NAMESPACE_MICROSOFT.1) {
        match prop.local_name {
            "Win32CreationTime" => {
                out.write(XmlWEvent::start_element((
                    WEBDAV_XML_NAMESPACE_MICROSOFT.0,
                    "Win32CreationTime",
                )))?;
                out.write(XmlWEvent::characters(&file_time_created(meta).to_rfc3339()))?;
            }
            "Win32FileAttributes" => {
                out.write(XmlWEvent::start_element((
                    WEBDAV_XML_NAMESPACE_MICROSOFT.0,
                    "Win32FileAttributes",
                )))?;
                out.write(XmlWEvent::characters(&format!(
                    "{:08x}",
                    win32_file_attributes(meta, path)
                )))?;
            }
            "Win32LastAccessTime" => {
                out.write(XmlWEvent::start_element((
                    WEBDAV_XML_NAMESPACE_MICROSOFT.0,
                    "Win32LastAccessTime",
                )))?;
                out.write(XmlWEvent::characters(
                    &file_time_accessed(meta).to_rfc3339(),
                ))?;
            }
            "Win32LastModifiedTime" => {
                out.write(XmlWEvent::start_element((
                    WEBDAV_XML_NAMESPACE_MICROSOFT.0,
                    "Win32LastModifiedTime",
                )))?;
                out.write(XmlWEvent::characters(
                    &file_time_modified(meta).to_rfc3339(),
                ))?;
            }
            _ => return Ok(false),
        }
    } else if prop.namespace == Some(WEBDAV_XML_NAMESPACE_APACHE.1) {
        match prop.local_name {
            "executable" => {
                out.write(XmlWEvent::start_element((
                    WEBDAV_XML_NAMESPACE_APACHE.0,
                    "executable",
                )))?;
                out.write(XmlWEvent::characters(if file_executable(meta) {
                    "T"
                } else {
                    "F"
                }))?;
            }
            _ => return Ok(false),
        }
    } else {
        return Ok(false);
    }

    out.write(XmlWEvent::end_element())?;
    Ok(true)
}

fn write_start_prop_element<W: Write>(
    out: &mut XmlWriter<W>,
    prop: XmlName,
) -> Result<(), xml::writer::Error> {
    if let Some(prop_namespace) = prop.namespace {
        if let Some(&(prefix, _)) = WEBDAV_XML_NAMESPACES
            .iter()
            .find(|(_, ns)| *ns == prop_namespace)
        {
            return out.write(XmlWEvent::start_element(XmlName {
                prefix: Some(prefix),
                ..prop
            }));
        }
        if prop
            .prefix
            .map(|pp| WEBDAV_XML_NAMESPACES.iter().any(|(pf, _)| *pf == pp))
            .unwrap_or(true)
        {
            return out.write(
                XmlWEvent::start_element(XmlName {
                    prefix: Some("U"),
                    ..prop
                })
                .ns("U", prop_namespace),
            );
        }
    }
    out.write(XmlWEvent::start_element(prop))
}

fn initialise_xml_output() -> Result<XmlWriter<Vec<u8>>, xml::writer::Error> {
    initialise_xml_output_with(vec![])
}

fn initialise_xml_output_with<W: Write>(output: W) -> Result<XmlWriter<W>, xml::writer::Error> {
    let mut out = XmlWriter::new_with_config(output, default_xml_emitter_config());
    out.write(XmlWEvent::StartDocument {
        version: XmlVersion::Version10,
        encoding: Some("utf-8"),
        standalone: None,
    })?;
    Ok(out)
}

// ─── PROPFIND parsing ──────────────────────────────────

#[derive(Debug, Clone)]
pub enum PropfindVariant {
    AllProp,
    PropName,
    Props(Vec<OwnedXmlName>),
}

impl std::fmt::Display for PropfindVariant {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            PropfindVariant::AllProp => f.write_str("all props"),
            PropfindVariant::PropName => f.write_str("prop names"),
            PropfindVariant::Props(props) => {
                for (i, name) in props.iter().enumerate() {
                    if i != 0 {
                        f.write_str(", ")?;
                    }
                    f.write_str(&name.local_name)?;
                }
                Ok(())
            }
        }
    }
}

enum PropfindParseError {
    EmptyBody,
    XmlError(String),
}

fn parse_propfind(body: &[u8]) -> Result<PropfindVariant, PropfindParseError> {
    if body.is_empty() {
        return Err(PropfindParseError::EmptyBody);
    }

    #[derive(Debug, Copy, Clone, PartialEq, Eq)]
    enum State {
        Start,
        PropFind,
        Prop,
        InProp,
    }

    let mut xml = XmlReader::new_with_config(body, default_xml_parser_config());
    let mut state = State::Start;
    let mut props = vec![];

    loop {
        let event = xml.next().map_err(|e| {
            if e.position() == xml::common::TextPosition::new()
                && e.msg().contains("no root element")
            {
                PropfindParseError::EmptyBody
            } else {
                PropfindParseError::XmlError(e.to_string())
            }
        })?;

        match (state, event) {
            (State::Start, XmlREvent::StartDocument { .. }) => (),
            (State::Start, XmlREvent::StartElement { ref name, .. })
                if name.local_name == "propfind" =>
            {
                state = State::PropFind
            }

            (State::PropFind, XmlREvent::StartElement { ref name, .. })
                if name.local_name == "allprop" =>
            {
                return Ok(PropfindVariant::AllProp);
            }
            (State::PropFind, XmlREvent::StartElement { ref name, .. })
                if name.local_name == "propname" =>
            {
                return Ok(PropfindVariant::PropName);
            }
            (State::PropFind, XmlREvent::StartElement { ref name, .. })
                if name.local_name == "prop" =>
            {
                state = State::Prop
            }

            (State::Prop, XmlREvent::StartElement { name, .. }) => {
                state = State::InProp;
                props.push(name);
            }
            (State::Prop, XmlREvent::EndElement { .. }) => {
                return Ok(PropfindVariant::Props(props));
            }

            (State::InProp, XmlREvent::EndElement { .. }) => state = State::Prop,

            (_, ev) => {
                return Err(PropfindParseError::XmlError(format!(
                    "Unexpected event {:?}",
                    ev
                )));
            }
        }
    }
}

// ─── PROPPATCH ─────────────────────────────────────────

#[derive(Debug, Default)]
#[allow(non_snake_case)]
struct ProppatchActionables {
    Win32CreationTime: Option<u64>,
    Win32LastAccessTime: Option<u64>,
    Win32LastModifiedTime: Option<u64>,
    executable: Option<bool>,
}

struct ProppatchProperty {
    name: OwnedXmlName,
    value: String,
    remove: bool,
}

fn prepare_proppatch(props: &[ProppatchProperty]) -> (ProppatchActionables, Vec<StatusCode>) {
    let mut actionables = ProppatchActionables::default();
    let mut statuses = Vec::with_capacity(props.len());
    for prop in props {
        let status = if prop.remove {
            StatusCode::FORBIDDEN
        } else if prop.name.namespace.as_deref() == Some(WEBDAV_XML_NAMESPACE_MICROSOFT.1) {
            match prop.name.local_name.as_str() {
                "Win32LastModifiedTime" | "Win32LastAccessTime" | "Win32CreationTime" => {
                    if prop.name.local_name == "Win32CreationTime" && !cfg!(windows) {
                        StatusCode::FORBIDDEN
                    } else if let Some(value) = win32time(&prop.value) {
                        match prop.name.local_name.as_str() {
                            "Win32LastModifiedTime" => {
                                actionables.Win32LastModifiedTime = Some(value)
                            }
                            "Win32LastAccessTime" => actionables.Win32LastAccessTime = Some(value),
                            _ => actionables.Win32CreationTime = Some(value),
                        }
                        StatusCode::OK
                    } else {
                        StatusCode::CONFLICT
                    }
                }
                _ => StatusCode::FORBIDDEN,
            }
        } else if cfg!(unix)
            && prop.name.namespace.as_deref() == Some(WEBDAV_XML_NAMESPACE_APACHE.1)
            && prop.name.local_name == "executable"
        {
            match prop.value.as_str() {
                "T" => {
                    actionables.executable = Some(true);
                    StatusCode::OK
                }
                "F" => {
                    actionables.executable = Some(false);
                    StatusCode::OK
                }
                _ => StatusCode::CONFLICT,
            }
        } else {
            StatusCode::FORBIDDEN
        };
        statuses.push(status);
    }
    if statuses.iter().any(|status| *status != StatusCode::OK) {
        for status in &mut statuses {
            if *status == StatusCode::OK {
                *status = StatusCode::FAILED_DEPENDENCY;
            }
        }
    }
    (actionables, statuses)
}

fn apply_proppatch(path: &Path, actionables: &ProppatchActionables) -> std::io::Result<()> {
    let permissions = fs::metadata(path)?.permissions();
    if let Some(executable) = actionables.executable {
        set_executable(path, executable)?;
    }
    // Each platform updates all requested timestamps in one system call. If it
    // fails after a chmod, restore the only preceding filesystem mutation.
    if let Err(error) = set_times(
        path,
        actionables.Win32LastModifiedTime,
        actionables.Win32LastAccessTime,
        actionables.Win32CreationTime,
    ) {
        if actionables.executable.is_some() {
            fs::set_permissions(path, permissions).map_err(|rollback| {
                std::io::Error::other(format!(
                    "property update failed: {error}; permission rollback failed: {rollback}"
                ))
            })?;
        }
        return Err(error);
    }
    Ok(())
}

/// Revalidate the current namespace while excluding this server's other mutations.
/// A path resolved before receiving the body is only a name, not a pinned object.
fn commit_proppatch(
    req: &Request,
    config: &Arc<AppConfig>,
    conditions: &WriteConditions,
    actionables: &ProppatchActionables,
    apply: bool,
) -> std::io::Result<Option<Response>> {
    let _guard = MUTATION_LOCK.blocking_lock();
    if let Some(response) = crate::hoops::auth::check_auth(req, config) {
        return Ok(Some(response));
    }
    let segments: Vec<_> = req
        .uri()
        .path()
        .split('/')
        .filter(|s| !s.is_empty())
        .collect();
    let (path, symlink, invalid) = resolve_path(
        &config.hosted_directory.1,
        &segments,
        config.follow_symlinks,
    );
    let mut response = Response::new();
    if invalid {
        set_error(
            &mut response,
            StatusCode::BAD_REQUEST,
            "400 Bad Request",
            "Percent-encoding decoded to invalid UTF-8.",
        );
        return Ok(Some(response));
    }
    if !path.exists() || config.is_symlink_denied(symlink, &path) {
        set_error(
            &mut response,
            StatusCode::NOT_FOUND,
            "404 Not Found",
            "The requested entity doesn't exist.",
        );
        return Ok(Some(response));
    }
    if !conditions.allows_path(&path)? {
        response.status_code(StatusCode::PRECONDITION_FAILED);
        return Ok(Some(response));
    }
    if apply {
        apply_proppatch(&path, actionables)?;
    }
    Ok(None)
}

#[handler]
pub async fn handle_proppatch(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let config = depot.get_typed::<Arc<AppConfig>>().unwrap().clone();

    if let Some(resp) = crate::hoops::auth::check_auth(req, &config) {
        *res = resp;
        return;
    }

    if config.writes_temp_dir.is_none() {
        set_error(
            res,
            StatusCode::FORBIDDEN,
            "403 Forbidden",
            "Write requests not allowed. Use -w.",
        );
        return;
    }

    let url_path_raw = req.uri().path().to_string();
    let segments: Vec<&str> = url_path_raw.split('/').filter(|s| !s.is_empty()).collect();
    let (req_p, symlink, url_err) = resolve_path(
        &config.hosted_directory.1,
        &segments,
        config.follow_symlinks,
    );

    if url_err {
        set_error(
            res,
            StatusCode::BAD_REQUEST,
            "400 Bad Request",
            "Percent-encoding decoded to invalid UTF-8.",
        );
        return;
    }

    if !req_p.exists() || config.is_symlink_denied(symlink, &req_p) {
        set_error(
            res,
            StatusCode::NOT_FOUND,
            "404 Not Found",
            "The requested entity doesn't exist.",
        );
        return;
    }

    let mut snapshot = Request::new();
    *snapshot.uri_mut() = req.uri().clone();
    *snapshot.method_mut() = req.method().clone();
    *snapshot.headers_mut() = req.headers().clone();
    *snapshot.remote_addr_mut() = req.remote_addr().clone();
    let Some(body_bytes) = control_body(req, res).await else {
        return;
    };
    let props = match parse_proppatch(&body_bytes) {
        Ok(pp) => pp,
        Err(e) => {
            let remote = req.remote_addr().to_string();
            log_msg(
                config.log,
                &format!(
                    "{} tried to PROPPATCH {} with invalid XML",
                    remote,
                    req_p.display()
                ),
            );
            set_error(
                res,
                StatusCode::BAD_REQUEST,
                "400 Bad Request",
                &format!("Invalid XML: {}", e),
            );
            return;
        }
    };

    let remote = req.remote_addr().to_string();
    log_msg(
        config.log,
        &format!("{} requested PROPPATCH on {}", remote, req_p.display()),
    );

    let conditions = match WriteConditions::from_request(req) {
        Ok(conditions) => conditions,
        Err(status) => {
            res.status_code(status);
            return;
        }
    };
    let (actionables, mut statuses) = prepare_proppatch(&props);
    let apply = statuses.iter().all(|status| *status == StatusCode::OK);
    let work_config = config.clone();
    let Some(job) = config.spawn_blocking(move || {
        commit_proppatch(&snapshot, &work_config, &conditions, &actionables, apply)
    }) else {
        res.status_code(StatusCode::SERVICE_UNAVAILABLE);
        return;
    };
    match job.await.unwrap_or_else(|error| Err(IoError::other(error))) {
        Ok(Some(response)) => {
            *res = response;
            return;
        }
        Ok(None) => {}
        Err(error) => {
            log_msg(config.log, &format!("PROPPATCH failed: {error}"));
            statuses.fill(if error.kind() == IoErrorKind::PermissionDenied {
                StatusCode::FORBIDDEN
            } else if error.kind() == IoErrorKind::Other {
                StatusCode::INTERNAL_SERVER_ERROR
            } else {
                StatusCode::CONFLICT
            });
        }
    }

    let url = req.uri().to_string();
    match write_proppatch_output(&props, &statuses, &url) {
        Ok(xml_bytes) => {
            res.status_code(StatusCode::MULTI_STATUS);
            res.headers_mut().insert(
                salvo::http::header::CONTENT_TYPE,
                "text/xml; charset=utf-8".parse().unwrap(),
            );
            res.headers_mut()
                .insert(salvo::http::header::SERVER, USER_AGENT.parse().unwrap());
            res.write_body(xml_bytes).ok();
        }
        Err(e) => {
            set_error(
                res,
                StatusCode::INTERNAL_SERVER_ERROR,
                "500 Internal Server Error",
                &format!("XML error: {}", e),
            );
        }
    }
}

fn parse_proppatch(body: &[u8]) -> Result<Vec<ProppatchProperty>, String> {
    #[derive(Debug, Copy, Clone, PartialEq, Eq)]
    enum State {
        Start,
        PropertyUpdate,
        Action,
        Prop,
        InProp,
        Done,
    }

    let mut xml = XmlReader::new_with_config(body, default_xml_parser_config());
    let mut state = State::Start;
    let mut props = vec![];
    let mut propname = None;
    let mut is_remove = false;
    let mut propdata = String::new();

    loop {
        let event = xml.next().map_err(|e| e.to_string())?;

        match (state, event) {
            (State::Start, XmlREvent::StartDocument { .. }) => (),
            (State::Start, XmlREvent::StartElement { ref name, .. })
                if name.local_name == "propertyupdate"
                    && name.namespace.as_deref() == Some("DAV:") =>
            {
                state = State::PropertyUpdate
            }

            (State::PropertyUpdate, XmlREvent::StartElement { ref name, .. })
                if name.local_name == "set" && name.namespace.as_deref() == Some("DAV:") =>
            {
                state = State::Action;
                is_remove = false;
            }
            (State::PropertyUpdate, XmlREvent::StartElement { ref name, .. })
                if name.local_name == "remove" && name.namespace.as_deref() == Some("DAV:") =>
            {
                state = State::Action;
                is_remove = true;
            }
            (State::PropertyUpdate, XmlREvent::EndElement { .. }) => {
                state = State::Done;
            }
            (State::Done, XmlREvent::EndDocument) if !props.is_empty() => return Ok(props),

            (State::Action, XmlREvent::StartElement { ref name, .. })
                if name.local_name == "prop" && name.namespace.as_deref() == Some("DAV:") =>
            {
                state = State::Prop
            }
            (State::Action, XmlREvent::EndElement { .. }) => state = State::PropertyUpdate,

            (State::Prop, XmlREvent::StartElement { name, .. }) => {
                state = State::InProp;
                propname = Some(name);
            }
            (State::Prop, XmlREvent::EndElement { .. }) => state = State::Action,

            (State::InProp, XmlREvent::EndElement { name, .. }) => {
                if Some(&name) == propname.as_ref() {
                    props.push(ProppatchProperty {
                        name,
                        value: std::mem::take(&mut propdata),
                        remove: is_remove,
                    });
                    state = State::Prop;
                }
            }
            (State::InProp, XmlREvent::Characters(data)) if !is_remove => {
                propdata.push_str(&data);
            }
            (_, XmlREvent::Comment(_)) => {}

            (_, ev) => return Err(format!("Unexpected event {:?}", ev)),
        }
    }
}

fn write_proppatch_output(
    props: &[ProppatchProperty],
    statuses: &[StatusCode],
    req_url: &str,
) -> Result<Vec<u8>, String> {
    let mut out = initialise_xml_output().map_err(|e| e.to_string())?;

    let mut start = XmlWEvent::start_element("D:multistatus")
        .ns(WEBDAV_XML_NAMESPACES[0].0, WEBDAV_XML_NAMESPACES[0].1);
    for &(prefix, namespace) in &WEBDAV_XML_NAMESPACES[1..] {
        start = start.ns(prefix, namespace);
    }
    out.write(start).map_err(|e| e.to_string())?;
    out.write(XmlWEvent::start_element("D:response"))
        .map_err(|e| e.to_string())?;

    out.write(XmlWEvent::start_element("D:href"))
        .map_err(|e| e.to_string())?;
    out.write(XmlWEvent::characters(req_url))
        .map_err(|e| e.to_string())?;
    out.write(XmlWEvent::end_element())
        .map_err(|e| e.to_string())?;

    for (prop, status) in props.iter().zip(statuses) {
        out.write(XmlWEvent::start_element("D:propstat"))
            .map_err(|e| e.to_string())?;
        out.write(XmlWEvent::start_element("D:prop"))
            .map_err(|e| e.to_string())?;
        write_start_prop_element(&mut out, prop.name.borrow()).map_err(|e| e.to_string())?;
        out.write(XmlWEvent::end_element())
            .map_err(|e| e.to_string())?;
        out.write(XmlWEvent::end_element())
            .map_err(|e| e.to_string())?;
        out.write(XmlWEvent::start_element("D:status"))
            .map_err(|e| e.to_string())?;
        out.write(XmlWEvent::characters(&format!(
            "HTTP/1.1 {} {}",
            status.as_u16(),
            status.canonical_reason().unwrap_or("Unknown")
        )))
        .map_err(|e| e.to_string())?;
        out.write(XmlWEvent::end_element())
            .map_err(|e| e.to_string())?;

        out.write(XmlWEvent::end_element())
            .map_err(|e| e.to_string())?; // propstat
    }
    out.write(XmlWEvent::end_element())
        .map_err(|e| e.to_string())?; // response
    out.write(XmlWEvent::end_element())
        .map_err(|e| e.to_string())?; // multistatus

    Ok(out.into_inner())
}

// ─── MKCOL ─────────────────────────────────────────────

#[handler]
pub async fn handle_mkcol(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let config = depot.get_typed::<Arc<AppConfig>>().unwrap().clone();

    if let Some(resp) = crate::hoops::auth::check_auth(req, &config) {
        *res = resp;
        return;
    }

    let url_path_raw = req.uri().path().to_string();
    let segments: Vec<&str> = url_path_raw.split('/').filter(|s| !s.is_empty()).collect();
    let (req_p, symlink, url_err) = resolve_path(
        &config.hosted_directory.1,
        &segments,
        config.follow_symlinks,
    );

    let remote = req.remote_addr().to_string();
    log_msg(
        config.log,
        &format!("{} requested to MKCOL at {}", remote, req_p.display()),
    );

    if url_err {
        set_error(
            res,
            StatusCode::BAD_REQUEST,
            "400 Bad Request",
            "Percent-encoding decoded to invalid UTF-8.",
        );
        return;
    }

    if config.writes_temp_dir.is_none() {
        set_error(
            res,
            StatusCode::FORBIDDEN,
            "403 Forbidden",
            "Write requests not allowed. Use -w.",
        );
        return;
    }

    if !req_p.parent().map(|pp| pp.exists()).unwrap_or(true)
        || config.is_symlink_denied(symlink, &req_p)
    {
        res.status_code(StatusCode::CONFLICT);
        return;
    }

    // Check for non-empty body
    let Some(body) = control_body(req, res).await else {
        return;
    };
    if !body.is_empty() {
        res.status_code(StatusCode::UNSUPPORTED_MEDIA_TYPE);
        return;
    }

    let conditions = match WriteConditions::from_request(req) {
        Ok(conditions) => conditions,
        Err(status) => {
            res.status_code(status);
            return;
        }
    };
    let Some(job) = config.spawn_blocking(move || {
        let _guard = MUTATION_LOCK.blocking_lock();
        if !conditions.allows_path(&req_p)? {
            return Ok(false);
        }
        fs::create_dir(&req_p)?;
        Ok(true)
    }) else {
        res.status_code(StatusCode::SERVICE_UNAVAILABLE);
        return;
    };
    match job.await.unwrap_or_else(|error| Err(IoError::other(error))) {
        Ok(true) => {
            res.status_code(StatusCode::CREATED);
        }
        Ok(false) => {
            res.status_code(StatusCode::PRECONDITION_FAILED);
        }
        Err(e) => match e.kind() {
            IoErrorKind::NotFound => {
                res.status_code(StatusCode::CONFLICT);
            }
            IoErrorKind::AlreadyExists => {
                res.status_code(StatusCode::METHOD_NOT_ALLOWED);
                res.render(Text::Plain("File exists"));
            }
            _ => {
                res.status_code(StatusCode::FORBIDDEN);
            }
        },
    }
}

// ─── COPY ──────────────────────────────────────────────

#[handler]
pub async fn handle_copy(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    handle_copy_move(req, depot, res, false).await;
}

// ─── MOVE ──────────────────────────────────────────────

#[handler]
pub async fn handle_move(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    handle_copy_move(req, depot, res, true).await;
}

async fn handle_copy_move(req: &mut Request, depot: &mut Depot, res: &mut Response, is_move: bool) {
    let config = depot.get_typed::<Arc<AppConfig>>().unwrap().clone();
    if let Some(response) = crate::hoops::auth::check_auth(req, &config) {
        *res = response;
        return;
    }
    let mut snapshot = Request::new();
    *snapshot.uri_mut() = req.uri().clone();
    *snapshot.headers_mut() = req.headers().clone();
    *snapshot.method_mut() = req.method().clone();
    let work_config = config.clone();
    let Some(job) = config.spawn_blocking(move || {
        let mut response = Response::new();
        copy_move_work(&mut snapshot, &work_config, &mut response, is_move);
        response
    }) else {
        res.status_code(StatusCode::SERVICE_UNAVAILABLE);
        return;
    };
    match job.await {
        Ok(response) => *res = response,
        Err(_) => {
            res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
        }
    }
}

fn copy_move_work(req: &mut Request, config: &Arc<AppConfig>, res: &mut Response, is_move: bool) {
    if let Some(resp) = crate::hoops::auth::check_auth(req, config) {
        *res = resp;
        return;
    }

    let url_path_raw = req.uri().path().to_string();
    let segments: Vec<&str> = url_path_raw.split('/').filter(|s| !s.is_empty()).collect();
    let (req_p, symlink, url_err) = resolve_path(
        &config.hosted_directory.1,
        &segments,
        config.follow_symlinks,
    );

    if url_err {
        set_error(
            res,
            StatusCode::BAD_REQUEST,
            "400 Bad Request",
            "Percent-encoding decoded to invalid UTF-8.",
        );
        return;
    }

    // Parse Destination header
    let dest_url = match req
        .headers()
        .get("Destination")
        .and_then(|v| v.to_str().ok())
    {
        Some(d) => d.to_string(),
        None => {
            set_error(
                res,
                StatusCode::BAD_REQUEST,
                "400 Bad Request",
                "Destination URL invalid or nonexistent.",
            );
            return;
        }
    };

    let Some(dest_path_str) = local_destination_path(req, &dest_url) else {
        set_error(
            res,
            StatusCode::BAD_REQUEST,
            "400 Bad Request",
            "Destination must be a local resource URL without a query.",
        );
        return;
    };
    if let Some(response) = crate::hoops::auth::check_auth_path(req, config, &dest_path_str) {
        *res = response;
        return;
    }
    let dest_segments: Vec<&str> = dest_path_str
        .split('/')
        .filter(|s: &&str| !s.is_empty())
        .collect();
    let (dest_p, dest_symlink, dest_url_err) = resolve_path(
        &config.hosted_directory.1,
        &dest_segments,
        config.follow_symlinks,
    );

    if dest_url_err {
        set_error(
            res,
            StatusCode::BAD_REQUEST,
            "400 Bad Request",
            "Percent-encoding decoded destination to invalid UTF-8.",
        );
        return;
    }

    let (depth, overwrite) = match (
        control_header(req, "Depth", Depth::Infinity, Depth::parse),
        control_header(req, "Overwrite", Overwrite::default(), Overwrite::parse),
    ) {
        (Ok(depth), Ok(overwrite)) => (depth, overwrite.0),
        _ => {
            set_error(
                res,
                StatusCode::BAD_REQUEST,
                "400 Bad Request",
                "Invalid or repeated Depth/Overwrite header.",
            );
            return;
        }
    };
    if req_p.is_dir()
        && ((is_move && depth != Depth::Infinity) || (!is_move && depth == Depth::One))
    {
        set_error(
            res,
            StatusCode::BAD_REQUEST,
            "400 Bad Request",
            "MOVE requires Depth: infinity; COPY requires Depth: 0 or infinity.",
        );
        return;
    }

    let remote = req.remote_addr().to_string();
    log_msg(
        config.log,
        &format!(
            "{} requested to {}{} {} to {} at depth {}",
            remote,
            if overwrite { "overwrite-" } else { "" },
            if is_move { "MOVE" } else { "COPY" },
            req_p.display(),
            dest_p.display(),
            depth
        ),
    );

    if config.writes_temp_dir.is_none() {
        set_error(
            res,
            StatusCode::FORBIDDEN,
            "403 Forbidden",
            "Write requests not allowed. Use -w.",
        );
        return;
    }

    if req_p == dest_p {
        res.status_code(StatusCode::FORBIDDEN);
        return;
    }

    if !req_p.exists() || config.is_symlink_denied(symlink, &req_p) {
        set_error(
            res,
            StatusCode::NOT_FOUND,
            "404 Not Found",
            "The requested entity doesn't exist.",
        );
        return;
    }

    if !dest_p.parent().map(|pp| pp.exists()).unwrap_or(true)
        || config.is_symlink_denied(dest_symlink, &dest_p)
    {
        res.status_code(StatusCode::CONFLICT);
        return;
    }

    let mut overwritten = false;
    if req_p.is_dir() && (depth != Depth::Zero || is_move) {
        for path in [&url_path_raw, &dest_path_str] {
            if let Some(response) = crate::hoops::auth::check_tree_auth(req, config, path) {
                *res = response;
                return;
            }
        }
    }
    let conditions = match WriteConditions::from_request(req) {
        Ok(conditions) => conditions,
        Err(status) => {
            res.status_code(status);
            return;
        }
    };
    let _mutation_lock = MUTATION_LOCK.blocking_lock();
    match conditions.allows_path(&req_p) {
        Ok(true) => {}
        Ok(false) => {
            res.status_code(StatusCode::PRECONDITION_FAILED);
            return;
        }
        Err(_) => {
            res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
            return;
        }
    }
    if dest_p.exists() {
        if !overwrite {
            res.status_code(StatusCode::PRECONDITION_FAILED);
            return;
        }
        // Preserve the existing collection until the complete copy is ready.
        // Retain the current policy of refusing nonempty destination collections.
        if !is_move
            && dest_p.is_dir()
            && !fs::read_dir(&dest_p).is_ok_and(|mut entries| entries.next().is_none())
        {
            res.status_code(StatusCode::LOCKED);
            return;
        }
        overwritten = true;
    }

    if is_move {
        // A failed rename leaves the complete source in place. In particular,
        // never emulate cross-device MOVE by deleting a partially copied tree.
        let result = if overwrite {
            fs::rename(&req_p, &dest_p)
        } else {
            crate::util::namespace::rename_no_replace(&req_p, &dest_p)
        };
        copy_response(res, result, overwritten);
        return;
    }

    if !overwrite {
        copy_response(
            res,
            crate::util::namespace::copy_no_replace(&req_p, &dest_p, depth == Depth::Infinity),
            false,
        );
        return;
    }

    let result = crate::util::namespace::copy_replace(&req_p, &dest_p, depth == Depth::Infinity);
    if let Err(error) = &result {
        log_msg(config.log, &format!("COPY failed: {error}"));
    }
    copy_response(res, result, overwritten);
}

fn control_header<T>(
    req: &Request,
    name: &str,
    default: T,
    parse: fn(&str) -> Option<T>,
) -> Result<T, ()> {
    let mut values = req.headers().get_all(name).iter();
    let Some(value) = values.next() else {
        return Ok(default);
    };
    if values.next().is_some() {
        return Err(());
    }
    value.to_str().ok().and_then(parse).ok_or(())
}

fn local_destination_path(req: &Request, destination: &str) -> Option<String> {
    let uri: salvo::http::uri::Uri = destination.parse().ok()?;
    if uri.query().is_some() || destination.starts_with("//") {
        return None;
    }
    if let Some(authority) = uri.authority() {
        let scheme = uri.scheme_str()?;
        let request_scheme = req.uri().scheme_str().unwrap_or("http");
        if !matches!(scheme, "http" | "https") || scheme != request_scheme {
            return None;
        }
        let request_authority = req
            .uri()
            .authority()
            .cloned()
            .or_else(|| req.headers().get("Host")?.to_str().ok()?.parse().ok())?;
        let default_port = if scheme == "https" { 443 } else { 80 };
        if !authority
            .host()
            .eq_ignore_ascii_case(request_authority.host())
            || authority.port_u16().unwrap_or(default_port)
                != request_authority.port_u16().unwrap_or(default_port)
        {
            return None;
        }
    } else if uri.scheme().is_some() || !destination.starts_with('/') {
        return None;
    }
    Some(uri.path().to_string())
}

fn copy_response(res: &mut Response, op_result: std::io::Result<()>, overwritten: bool) {
    match op_result {
        Ok(_) => {
            if overwritten {
                res.status_code(StatusCode::NO_CONTENT);
            } else {
                res.status_code(StatusCode::CREATED);
            }
        }
        Err(error) if error.kind() == IoErrorKind::AlreadyExists => {
            res.status_code(StatusCode::PRECONDITION_FAILED);
        }
        Err(_) => {
            res.status_code(StatusCode::INSUFFICIENT_STORAGE);
        }
    }
}

// ─── Helpers ───────────────────────────────────────────

fn set_error(res: &mut Response, status: StatusCode, title: &str, msg: &str) {
    let body = error_html_text(title, msg, "");
    res.status_code(status);
    res.headers_mut().insert(
        salvo::http::header::CONTENT_TYPE,
        "text/html; charset=utf-8".parse().unwrap(),
    );
    res.headers_mut()
        .insert(salvo::http::header::SERVER, USER_AGENT.parse().unwrap());
    res.render(Text::Html(body));
}

#[cfg(test)]
mod remaining_mutation_condition_tests {
    use salvo::test::RequestBuilder;

    use super::*;
    use crate::test_support::{TestDir, config};

    const UPDATE: &str = r#"<D:propertyupdate xmlns:D="DAV:" xmlns:Z="urn:schemas-microsoft-com:"><D:set><D:prop><Z:Win32LastModifiedTime>2020-09-13T12:26:40Z</Z:Win32LastModifiedTime></D:prop></D:set></D:propertyupdate>"#;

    #[tokio::test]
    async fn mkcol_checks_version_conditions_before_creating_a_collection() {
        let fixture = TestDir::new();
        let service = Service::new(crate::build_router(config(fixture.path(), &["-w", "-d"])));
        let response = RequestBuilder::new("http://localhost/collection", "MKCOL".parse().unwrap())
            .add_header("If-Match", "*", true)
            .send(&service)
            .await;
        assert_eq!(response.status_code, Some(StatusCode::PRECONDITION_FAILED));
        assert!(!fixture.path().join("collection").exists());
        for expected in [StatusCode::CREATED, StatusCode::PRECONDITION_FAILED] {
            let response =
                RequestBuilder::new("http://localhost/collection", "MKCOL".parse().unwrap())
                    .add_header("If-None-Match", "*", true)
                    .send(&service)
                    .await;
            assert_eq!(response.status_code, Some(expected));
        }
        assert!(fixture.path().join("collection").is_dir());
    }

    #[tokio::test]
    async fn stale_property_conditions_leave_data_and_timestamps_unchanged() {
        let fixture = TestDir::new();
        let path = fixture.path().join("file");
        fs::write(&path, b"original data").unwrap();
        let metadata = fs::metadata(&path).unwrap();
        let before = metadata.modified().unwrap();
        let tag = format!("\"{}\"", path_metadata_and_etag(&path).unwrap().1);
        let service = Service::new(crate::build_router(config(fixture.path(), &["-w", "-d"])));
        for (header, value) in [
            ("If-Match", "\"stale\"".to_owned()),
            ("If-Match", format!("W/{tag}")),
            ("If-None-Match", "*".to_owned()),
            (
                "If-Unmodified-Since",
                "Thu, 01 Jan 1970 00:00:00 GMT".to_owned(),
            ),
        ] {
            let response =
                RequestBuilder::new("http://localhost/file", "PROPPATCH".parse().unwrap())
                    .add_header(header, value, true)
                    .body(UPDATE)
                    .send(&service)
                    .await;
            assert_eq!(response.status_code, Some(StatusCode::PRECONDITION_FAILED));
            assert_eq!(fs::read(&path).unwrap(), b"original data");
            assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), before);
        }
        let response = RequestBuilder::new("http://localhost/file", "PROPPATCH".parse().unwrap())
            .add_header("If-Match", tag, true)
            .add_header("If-Unmodified-Since", "Thu, 01 Jan 1970 00:00:00 GMT", true)
            .body(UPDATE)
            .send(&service)
            .await;
        assert_eq!(response.status_code, Some(StatusCode::MULTI_STATUS));
        assert_ne!(fs::metadata(&path).unwrap().modified().unwrap(), before);
        assert_eq!(fs::read(&path).unwrap(), b"original data");
    }

    #[tokio::test]
    async fn mkcol_requires_bounded_worker_capacity_and_the_mutation_lock() {
        let fixture = TestDir::new();
        let config = config(fixture.path(), &["-w", "-d"]);
        let service = Service::new(crate::build_router(config.clone()));
        let permits = config
            .blocking_jobs
            .clone()
            .acquire_many_owned(4)
            .await
            .unwrap();
        let response = RequestBuilder::new("http://localhost/collection", "MKCOL".parse().unwrap())
            .send(&service)
            .await;
        assert_eq!(response.status_code, Some(StatusCode::SERVICE_UNAVAILABLE));
        assert!(!fixture.path().join("collection").exists());
        drop(permits);
        let guard = MUTATION_LOCK.lock().await;
        let request = tokio::spawn(async move {
            RequestBuilder::new("http://localhost/collection", "MKCOL".parse().unwrap())
                .send(&service)
                .await
        });
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while config.blocking_jobs.available_permits() == 4 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(!request.is_finished());
        assert!(!fixture.path().join("collection").exists());
        drop(guard);
        assert_eq!(
            request.await.unwrap().status_code,
            Some(StatusCode::CREATED)
        );
    }
}

#[cfg(test)]
mod overwrite_copy_tests {
    use salvo::test::RequestBuilder;

    use super::*;
    use crate::test_support::{TestDir, config};

    #[tokio::test]
    async fn descendant_copy_failure_keeps_existing_target_and_success_replaces_complete_content() {
        let fixture = TestDir::new();
        fs::create_dir(fixture.path().join("source")).unwrap();
        fs::write(fixture.path().join("source/payload"), b"new payload").unwrap();
        fs::create_dir(fixture.path().join("source/child")).unwrap();
        fs::create_dir(fixture.path().join("target")).unwrap();
        let service = Service::new(crate::build_router(config(fixture.path(), &["-w", "-d"])));
        let response = RequestBuilder::new("http://localhost/source", "COPY".parse().unwrap())
            .add_header("Destination", "/source/child", true)
            .add_header("Overwrite", "T", true)
            .send(&service)
            .await;
        assert!(!response.status_code.unwrap().is_success());
        assert!(fixture.path().join("source/child").is_dir());
        assert_eq!(
            fs::read(fixture.path().join("source/payload")).unwrap(),
            b"new payload"
        );
        let response = RequestBuilder::new("http://localhost/source", "COPY".parse().unwrap())
            .add_header("Destination", "/target", true)
            .add_header("Overwrite", "T", true)
            .send(&service)
            .await;
        assert_eq!(response.status_code, Some(StatusCode::NO_CONTENT));
        assert_eq!(
            fs::read(fixture.path().join("target/payload")).unwrap(),
            b"new payload"
        );
        assert!(fixture.path().join("target/child").is_dir());
        assert!(
            fs::read_dir(fixture.path())
                .unwrap()
                .all(|entry| !is_internal_name(&entry.unwrap().file_name()))
        );
    }
}

#[cfg(test)]
mod href_tests {
    use salvo::test::{RequestBuilder, ResponseExt, TestClient};

    use super::*;
    use crate::test_support::{TestDir, config};

    #[tokio::test]
    async fn collection_hrefs_round_trip_encoded_names_without_queries() {
        let directory = TestDir::new();
        let parent = directory.path().join("space % directory");
        fs::create_dir(&parent).unwrap();
        let names = [
            "child space.txt",
            "literal%20.txt",
            "hash#.txt",
            "amp&.txt",
            "中文.txt",
        ];
        for name in names {
            fs::write(parent.join(name), name).unwrap();
        }
        let service = Service::new(crate::build_router(config(directory.path(), &["-d"])));
        for body in [
            "",
            r#"<D:propfind xmlns:D="DAV:"><D:prop><D:getcontentlength/></D:prop></D:propfind>"#,
        ] {
            let mut response = RequestBuilder::new(
                "http://localhost/space%20%25%20directory/?not-a-path=secret",
                "PROPFIND".parse().unwrap(),
            )
            .add_header("Depth", "1", true)
            .body(body)
            .send(&service)
            .await;
            assert_eq!(response.status_code, Some(StatusCode::MULTI_STATUS));
            let xml = response.take_string().await.unwrap();
            let mut in_href = false;
            let mut hrefs = Vec::new();
            for event in XmlReader::from_str(&xml) {
                match event.unwrap() {
                    XmlREvent::StartElement { name, .. } if name.local_name == "href" => {
                        in_href = true
                    }
                    XmlREvent::EndElement { name } if name.local_name == "href" => in_href = false,
                    XmlREvent::Characters(value) if in_href => hrefs.push(value),
                    _ => {}
                }
            }
            assert_eq!(hrefs.len(), names.len() + 1);
            for href in &hrefs {
                assert!(href.starts_with("/space%20%25%20directory/"), "{href}");
                assert!(!href.contains('?'), "query leaked into {href}");
                assert!(
                    !href.contains("%2520directory"),
                    "double encoding in {href}"
                );
                let result = TestClient::get(format!("http://localhost{href}"))
                    .send(&service)
                    .await;
                assert_eq!(result.status_code, Some(StatusCode::OK), "{href}");
            }
            for name in names {
                let suffix = percent_encoding::utf8_percent_encode(name, HREF_SEGMENT).to_string();
                assert!(hrefs.iter().any(|href| href.ends_with(&suffix)), "{name}");
            }
        }
    }
}

#[cfg(test)]
mod control_body_tests {
    use std::pin::Pin;
    use std::task::{Context, Poll};

    use salvo::hyper::body::{Body, Bytes, Frame};
    use salvo::test::RequestBuilder;

    use super::*;
    use crate::test_support::{TestDir, config};

    #[tokio::test]
    async fn oversized_control_bodies_never_become_empty_requests() {
        let directory = TestDir::new();
        let source = directory.path().join("file");
        fs::write(&source, b"unchanged").unwrap();
        let before = fs::metadata(&source).unwrap().modified().unwrap();
        let service = Service::new(crate::build_router(config(directory.path(), &["-w", "-d"])));
        for (method, path) in [
            ("MKCOL", "/collection"),
            ("PROPFIND", "/"),
            ("PROPPATCH", "/file"),
        ] {
            let response =
                RequestBuilder::new(format!("http://localhost{path}"), method.parse().unwrap())
                    .add_header("Depth", "0", true)
                    .add_header("Content-Type", "application/xml", true)
                    .body(vec![b'x'; CONTROL_BODY_LIMIT + 1])
                    .send(&service)
                    .await;
            assert_eq!(
                response.status_code,
                Some(StatusCode::PAYLOAD_TOO_LARGE),
                "{method}"
            );
        }
        assert!(!directory.path().join("collection").exists());
        assert_eq!(fs::read(&source).unwrap(), b"unchanged");
        assert_eq!(fs::metadata(&source).unwrap().modified().unwrap(), before);
        let response = RequestBuilder::new("http://localhost/small", "MKCOL".parse().unwrap())
            .body("not empty")
            .send(&service)
            .await;
        assert_eq!(
            response.status_code,
            Some(StatusCode::UNSUPPORTED_MEDIA_TYPE)
        );
        assert!(!directory.path().join("small").exists());
    }

    struct BrokenBody(bool);

    impl Body for BrokenBody {
        type Data = Bytes;
        type Error = salvo::BoxedError;

        fn poll_frame(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
        ) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
            if self.0 {
                Poll::Pending
            } else {
                Poll::Ready(Some(Err(Box::new(std::io::Error::new(
                    std::io::ErrorKind::ConnectionReset,
                    "interrupted",
                )))))
            }
        }
    }

    #[tokio::test]
    async fn interrupted_and_stalled_control_bodies_are_errors() {
        for (pending, status) in [
            (false, StatusCode::BAD_REQUEST),
            (true, StatusCode::REQUEST_TIMEOUT),
        ] {
            let mut request = Request::new();
            *request.body_mut() = salvo::http::ReqBody::Boxed {
                inner: Box::pin(BrokenBody(pending)),
                fuse_config: None,
            };
            let mut response = Response::new();
            assert!(
                control_body_with_timeout(
                    &mut request,
                    &mut response,
                    std::time::Duration::from_millis(5)
                )
                .await
                .is_none()
            );
            assert_eq!(response.status_code, Some(status));
        }
    }
}

#[cfg(test)]
mod move_tests {
    use salvo::test::RequestBuilder;

    use super::*;
    use crate::test_support::{TestDir, config};

    #[tokio::test]
    async fn moves_complete_directory_and_keeps_source_on_failure() {
        let directory = TestDir::new();
        fs::create_dir_all(directory.path().join("source/nested")).unwrap();
        fs::write(directory.path().join("source/nested/file"), b"fixture").unwrap();
        let service = Service::new(crate::build_router(config(directory.path(), &["-w", "-d"])));
        let response = RequestBuilder::new(
            "http://localhost/source",
            salvo::http::Method::from_bytes(b"MOVE").unwrap(),
        )
        .add_header("Destination", "/destination", true)
        .send(&service)
        .await;
        assert_eq!(response.status_code, Some(StatusCode::CREATED));
        assert!(!directory.path().join("source").exists());
        assert_eq!(
            fs::read(directory.path().join("destination/nested/file")).unwrap(),
            b"fixture"
        );
        let response = RequestBuilder::new(
            "http://localhost/destination",
            salvo::http::Method::from_bytes(b"MOVE").unwrap(),
        )
        .add_header("Destination", "/destination/child", true)
        .send(&service)
        .await;
        assert!(!response.status_code.unwrap().is_success());
        assert_eq!(
            fs::read(directory.path().join("destination/nested/file")).unwrap(),
            b"fixture"
        );
    }
}

#[cfg(test)]
mod proppatch_tests {
    use salvo::test::{RequestBuilder, ResponseExt};

    use super::*;
    use crate::test_support::{TestDir, config};

    fn request(body: &str) -> RequestBuilder {
        RequestBuilder::new(
            "http://localhost/file",
            salvo::http::Method::from_bytes(b"PROPPATCH").unwrap(),
        )
        .add_header("Content-Type", "application/xml", true)
        .body(body.to_owned())
    }

    #[tokio::test]
    async fn commit_keeps_request_credentials_and_rejects_stale_conditions() {
        let directory = TestDir::new();
        let path = directory.path().join("file");
        fs::write(&path, b"fixture").unwrap();
        let service = Service::new(crate::build_router(config(
            directory.path(),
            &["-w", "-d", "--auth", "user:pass"],
        )));
        let body = r#"<D:propertyupdate xmlns:D="DAV:" xmlns:Z="urn:schemas-microsoft-com:"><D:set><D:prop><Z:Win32LastModifiedTime>2020-09-13T12:26:40Z</Z:Win32LastModifiedTime></D:prop></D:set></D:propertyupdate>"#;
        let mut response = request(body)
            .basic_auth("user", Some("pass"))
            .send(&service)
            .await;
        assert_eq!(response.status_code, Some(StatusCode::MULTI_STATUS));
        assert!(response.take_string().await.unwrap().contains("200 OK"));
        let before = fs::metadata(&path).unwrap().modified().unwrap();
        let response = request(body)
            .basic_auth("user", Some("pass"))
            .add_header("If-Match", r#""obsolete""#, true)
            .send(&service)
            .await;
        assert_eq!(response.status_code, Some(StatusCode::PRECONDITION_FAILED));
        assert_eq!(fs::metadata(path).unwrap().modified().unwrap(), before);
    }

    #[test]
    fn commit_rechecks_existence_after_namespace_changes() {
        let directory = TestDir::new();
        let path = directory.path().join("file");
        fs::write(&path, b"fixture").unwrap();
        let config = config(directory.path(), &["-w", "-d"]);
        let snapshot = request("").build();
        assert!(crate::hoops::auth::check_auth(&snapshot, &config).is_none());
        {
            let _guard = MUTATION_LOCK.blocking_lock();
            crate::util::namespace::rename_no_replace(&path, &directory.path().join("retired"))
                .unwrap();
        }
        let response = commit_proppatch(
            &snapshot,
            &config,
            &WriteConditions::default(),
            &ProppatchActionables::default(),
            true,
        )
        .unwrap()
        .unwrap();
        assert_eq!(response.status_code, Some(StatusCode::NOT_FOUND));
    }

    #[cfg(unix)]
    #[test]
    fn commit_revalidates_authorization_and_symlink_policy_after_namespace_publication() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        for policy in ["auth", "no-follow", "sandbox"] {
            let directory = TestDir::new();
            let root = directory.path().join("served");
            fs::create_dir_all(root.join("active")).unwrap();
            fs::create_dir(root.join("prepared")).unwrap();
            fs::create_dir(root.join("private")).unwrap();
            fs::write(root.join("active/file"), b"initial resource").unwrap();
            let target = if policy == "sandbox" {
                directory.path().join("outside")
            } else {
                root.join("private/file")
            };
            fs::write(&target, b"unchanged resource").unwrap();
            symlink(&target, root.join("prepared/file")).unwrap();
            let flags = match policy {
                "auth" => vec!["-w", "-d", "--path-auth", "private=user:pass"],
                "no-follow" => vec!["-w", "-d", "-s"],
                _ => vec!["-w", "-d", "-r"],
            };
            let config = config(&root, &flags);
            let snapshot =
                RequestBuilder::new("http://localhost/active/file", "PROPPATCH".parse().unwrap())
                    .build();
            assert!(crate::hoops::auth::check_auth(&snapshot, &config).is_none());
            let (initial, link, invalid) = resolve_path(
                &config.hosted_directory.1,
                &["active", "file"],
                config.follow_symlinks,
            );
            assert!(!invalid && initial.exists() && !config.is_symlink_denied(link, &initial));
            let before = fs::metadata(&target).unwrap();
            // Model a separate authorized namespace operation between the early
            // check and commit, using the same publication helpers and lock.
            {
                let _guard = MUTATION_LOCK.blocking_lock();
                crate::util::namespace::rename_no_replace(
                    &root.join("active"),
                    &root.join("retired"),
                )
                .unwrap();
                crate::util::namespace::copy_no_replace(
                    &root.join("prepared"),
                    &root.join("active"),
                    true,
                )
                .unwrap();
            }
            let response = commit_proppatch(
                &snapshot,
                &config,
                &WriteConditions::default(),
                &ProppatchActionables {
                    Win32LastModifiedTime: Some(1_600_000_000_000),
                    executable: Some(true),
                    ..Default::default()
                },
                true,
            )
            .unwrap()
            .unwrap();
            assert_eq!(
                response.status_code,
                Some(if policy == "auth" {
                    StatusCode::UNAUTHORIZED
                } else {
                    StatusCode::NOT_FOUND
                }),
                "{policy}"
            );
            let after = fs::metadata(&target).unwrap();
            assert_eq!(
                after.modified().unwrap(),
                before.modified().unwrap(),
                "{policy}"
            );
            assert_eq!(
                after.permissions().mode(),
                before.permissions().mode(),
                "{policy}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn commit_applies_properties_to_the_freshly_resolved_authorized_target() {
        use std::os::unix::fs::symlink;

        let directory = TestDir::new();
        fs::create_dir(directory.path().join("active")).unwrap();
        fs::create_dir(directory.path().join("prepared")).unwrap();
        let original = directory.path().join("original");
        fs::write(&original, b"original").unwrap();
        symlink(&original, directory.path().join("active/file")).unwrap();
        fs::write(directory.path().join("prepared/file"), b"replacement").unwrap();
        let config = config(directory.path(), &["-w", "-d"]);
        let snapshot =
            RequestBuilder::new("http://localhost/active/file", "PROPPATCH".parse().unwrap())
                .build();
        let (initial, ..) = resolve_path(&config.hosted_directory.1, &["active", "file"], true);
        assert_eq!(initial, original);
        let before = fs::metadata(&original).unwrap().modified().unwrap();
        {
            let _guard = MUTATION_LOCK.blocking_lock();
            crate::util::namespace::rename_no_replace(
                &directory.path().join("active"),
                &directory.path().join("retired"),
            )
            .unwrap();
            crate::util::namespace::copy_no_replace(
                &directory.path().join("prepared"),
                &directory.path().join("active"),
                true,
            )
            .unwrap();
        }
        assert!(
            commit_proppatch(
                &snapshot,
                &config,
                &WriteConditions::default(),
                &ProppatchActionables {
                    Win32LastModifiedTime: Some(1_600_000_000_000),
                    ..Default::default()
                },
                true
            )
            .unwrap()
            .is_none()
        );
        assert_eq!(fs::metadata(original).unwrap().modified().unwrap(), before);
        assert_eq!(
            fs::metadata(directory.path().join("active/file"))
                .unwrap()
                .modified()
                .unwrap()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis(),
            1_600_000_000_000
        );
    }

    #[tokio::test]
    async fn property_changes_wait_for_the_conditional_mutation_boundary() {
        let directory = TestDir::new();
        let path = directory.path().join("file");
        fs::write(&path, b"fixture").unwrap();
        let before = fs::metadata(&path).unwrap().modified().unwrap();
        let config = config(directory.path(), &["-w", "-d"]);
        let service = Service::new(crate::build_router(config.clone()));
        let guard = crate::util::write_conditions::MUTATION_LOCK.lock().await;
        let task = tokio::spawn(async move {
            request(r#"<D:propertyupdate xmlns:D="DAV:" xmlns:Z="urn:schemas-microsoft-com:"><D:set><D:prop><Z:Win32LastModifiedTime>2020-09-13T12:26:40Z</Z:Win32LastModifiedTime></D:prop></D:set></D:propertyupdate>"#)
                .send(&service)
                .await
        });
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while config.blocking_jobs.available_permits() == 4 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(!task.is_finished());
        assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), before);
        drop(guard);
        assert_eq!(
            task.await.unwrap().status_code,
            Some(StatusCode::MULTI_STATUS)
        );
        assert_ne!(fs::metadata(&path).unwrap().modified().unwrap(), before);
    }

    #[tokio::test]
    async fn supported_update_returns_valid_multistatus_and_actual_success() {
        let directory = TestDir::new();
        fs::write(directory.path().join("file"), b"fixture").unwrap();
        let service = Service::new(crate::build_router(config(directory.path(), &["-w", "-d"])));
        let mut response = request(r#"<D:propertyupdate xmlns:D="DAV:" xmlns:Z="urn:schemas-microsoft-com:"><D:set><D:prop><Z:Win32LastModifiedTime>2020-09-13T12:26:40.123Z</Z:Win32LastModifiedTime></D:prop></D:set></D:propertyupdate>"#).send(&service).await;
        assert_eq!(response.status_code, Some(StatusCode::MULTI_STATUS));
        let xml = response.take_string().await.unwrap();
        let mut stack = Vec::new();
        let mut saw_response = false;
        for event in XmlReader::new(xml.as_bytes()) {
            match event.unwrap() {
                XmlREvent::StartElement { name, .. } => {
                    if name.local_name == "href" {
                        assert_eq!(stack.last().map(String::as_str), Some("response"));
                    }
                    if name.local_name == "propstat" {
                        assert_eq!(stack.last().map(String::as_str), Some("response"));
                    }
                    if name.local_name == "response" {
                        saw_response = true;
                    }
                    stack.push(name.local_name);
                }
                XmlREvent::EndElement { .. } => {
                    stack.pop();
                }
                _ => {}
            }
        }
        assert!(saw_response);
        assert!(xml.contains("HTTP/1.1 200 OK"));
        assert!(!xml.contains("409 Conflict"));
        let modified = fs::metadata(directory.path().join("file"))
            .unwrap()
            .modified()
            .unwrap();
        assert_eq!(
            modified
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis(),
            1_600_000_000_123
        );
    }

    #[tokio::test]
    async fn unsupported_property_prevents_every_change_in_the_request() {
        let directory = TestDir::new();
        let file = directory.path().join("file");
        fs::write(&file, b"fixture").unwrap();
        let original = fs::metadata(&file).unwrap().modified().unwrap();
        let service = Service::new(crate::build_router(config(directory.path(), &["-w", "-d"])));
        let mut response = request(r#"<D:propertyupdate xmlns:D="DAV:" xmlns:Z="urn:schemas-microsoft-com:"><D:set><D:prop><Z:Win32LastModifiedTime>2020-09-13T12:26:40Z</Z:Win32LastModifiedTime><D:Win32LastModifiedTime>2021-01-01T00:00:00Z</D:Win32LastModifiedTime></D:prop></D:set></D:propertyupdate>"#).send(&service).await;
        assert_eq!(response.status_code, Some(StatusCode::MULTI_STATUS));
        let xml = response.take_string().await.unwrap();
        assert!(xml.contains("403 Forbidden"));
        assert!(xml.contains("424 Failed Dependency"));
        assert!(!xml.contains("200 OK"));
        assert_eq!(fs::metadata(file).unwrap().modified().unwrap(), original);
    }

    #[test]
    fn invalid_values_and_live_property_removal_never_prepare_mutations() {
        let props = parse_proppatch(br#"<D:propertyupdate xmlns:D="DAV:" xmlns:Z="urn:schemas-microsoft-com:"><D:set><D:prop><Z:Win32LastModifiedTime>invalid</Z:Win32LastModifiedTime></D:prop></D:set><D:remove><D:prop><Z:Win32LastAccessTime/></D:prop></D:remove></D:propertyupdate>"#).unwrap();
        let (_, statuses) = prepare_proppatch(&props);
        assert_eq!(statuses, [StatusCode::CONFLICT, StatusCode::FORBIDDEN]);
        assert!(
            parse_proppatch(br#"<propertyupdate xmlns="wrong"><set/></propertyupdate>"#).is_err()
        );
        assert!(parse_proppatch(br#"<D:propertyupdate xmlns:D="DAV:"/>trailing"#).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn failed_timestamp_update_restores_prior_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let directory = TestDir::new();
        let file = directory.path().join("file");
        fs::write(&file, b"fixture").unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o640)).unwrap();
        let actionables = ProppatchActionables {
            executable: Some(true),
            Win32CreationTime: Some(1),
            ..Default::default()
        };
        assert!(apply_proppatch(&file, &actionables).is_err());
        assert_eq!(
            fs::metadata(file).unwrap().permissions().mode() & 0o777,
            0o640
        );
    }
}

#[cfg(test)]
mod depth_limit_tests {
    use salvo::prelude::*;
    use salvo::test::{RequestBuilder, ResponseExt};

    #[tokio::test]
    async fn propfind_requires_finite_depth_before_traversing() {
        let dir = crate::test_support::TestDir::new();
        std::fs::create_dir(dir.path().join("child")).unwrap();
        std::fs::write(dir.path().join("child/grandchild.txt"), b"data").unwrap();
        let service = Service::new(crate::build_router(crate::test_support::config(
            dir.path(),
            &["-d"],
        )));
        for depth in ["infinity", "2", "invalid"] {
            let mut response =
                RequestBuilder::new("http://localhost/", "PROPFIND".parse().unwrap())
                    .add_header("Depth", depth, true)
                    .send(&service)
                    .await;
            assert_eq!(
                response.status_code,
                Some(if depth == "infinity" {
                    StatusCode::FORBIDDEN
                } else {
                    StatusCode::BAD_REQUEST
                })
            );
            if depth == "infinity" {
                assert!(
                    response
                        .take_string()
                        .await
                        .unwrap()
                        .contains("propfind-finite-depth")
                );
            }
        }
        let mut response = RequestBuilder::new("http://localhost/", "PROPFIND".parse().unwrap())
            .add_header("Depth", "1", true)
            .send(&service)
            .await;
        assert_eq!(response.status_code, Some(StatusCode::MULTI_STATUS));
        assert!(
            !response
                .take_string()
                .await
                .unwrap()
                .contains("grandchild.txt")
        );
    }
}

#[cfg(test)]
mod destination_auth_tests {
    use salvo::test::TestClient;

    use super::*;
    use crate::test_support::{TestDir, config};

    #[tokio::test]
    async fn copy_and_move_require_target_credentials_before_mutation() {
        let dir = TestDir::new();
        fs::create_dir(dir.path().join("private")).unwrap();
        fs::write(dir.path().join("public.txt"), "source").unwrap();
        fs::write(dir.path().join("private/target.txt"), "original").unwrap();
        let service = Service::new(crate::build_router(config(
            dir.path(),
            &["-w", "-d", "--path-auth", "private=user:pass"],
        )));
        for method in ["COPY", "MOVE"] {
            let mut request = TestClient::get("http://localhost/public.txt")
                .add_header("Destination", "/%70rivate/target.txt", true)
                .build();
            *request.method_mut() = method.parse().unwrap();
            let response = service.handle(request).await;
            assert_eq!(response.status_code, Some(StatusCode::UNAUTHORIZED));
            assert_eq!(
                fs::read_to_string(dir.path().join("private/target.txt")).unwrap(),
                "original"
            );
            assert!(dir.path().join("public.txt").exists());
        }
        let mut request = TestClient::get("http://localhost/public.txt")
            .basic_auth("user", Some("pass"))
            .add_header("Destination", "http://localhost/private/new.txt", true)
            .build();
        *request.method_mut() = "COPY".parse().unwrap();
        let response = service.handle(request).await;
        assert_eq!(response.status_code, Some(StatusCode::CREATED));
        assert_eq!(
            fs::read_to_string(dir.path().join("private/new.txt")).unwrap(),
            "source"
        );
    }

    #[test]
    fn destinations_must_have_the_request_origin_and_no_query() {
        let request = TestClient::get("http://localhost/source").build();
        assert_eq!(
            local_destination_path(&request, "/target"),
            Some("/target".into())
        );
        assert_eq!(
            local_destination_path(&request, "http://LOCALHOST:80/target"),
            Some("/target".into())
        );
        for destination in [
            "http://other.test/target",
            "http://localhost:81/target",
            "https://localhost/target",
            "/target?query",
            "//other.test/target",
            "relative/target",
            "ftp://localhost/target",
        ] {
            assert!(
                local_destination_path(&request, destination).is_none(),
                "{destination}"
            );
        }
    }
}

#[cfg(test)]
mod blocking_admission_tests {
    use salvo::test::RequestBuilder;

    use super::*;

    #[tokio::test]
    async fn busy_workers_do_not_mutate_copy_destinations_or_properties() {
        let dir = crate::test_support::TestDir::new();
        fs::write(dir.path().join("source"), b"fixture").unwrap();
        fs::create_dir(dir.path().join("destination")).unwrap();
        let before = fs::metadata(dir.path().join("source"))
            .unwrap()
            .modified()
            .unwrap();
        let config = crate::test_support::config(dir.path(), &["-w", "-d"]);
        let _all_jobs = config
            .blocking_jobs
            .clone()
            .acquire_many_owned(4)
            .await
            .unwrap();
        let service = Service::new(crate::build_router(config));
        let response = RequestBuilder::new("http://localhost/source", "COPY".parse().unwrap())
            .add_header("Destination", "/destination", true)
            .send(&service)
            .await;
        assert_eq!(response.status_code, Some(StatusCode::SERVICE_UNAVAILABLE));
        assert!(dir.path().join("destination").is_dir());
        let response = RequestBuilder::new("http://localhost/source", "PROPPATCH".parse().unwrap())
            .add_header("Content-Type", "application/xml", true)
            .body(r#"<D:propertyupdate xmlns:D="DAV:" xmlns:Z="urn:schemas-microsoft-com:"><D:set><D:prop><Z:Win32LastModifiedTime>2020-09-13T12:26:40Z</Z:Win32LastModifiedTime></D:prop></D:set></D:propertyupdate>"#)
            .send(&service).await;
        assert_eq!(response.status_code, Some(StatusCode::SERVICE_UNAVAILABLE));
        assert_eq!(
            fs::metadata(dir.path().join("source"))
                .unwrap()
                .modified()
                .unwrap(),
            before
        );
    }
}

#[cfg(test)]
mod mutation_precondition_tests {
    use salvo::test::{RequestBuilder, TestClient};

    use super::*;
    use crate::test_support::{TestDir, config};

    #[tokio::test]
    async fn failed_conditions_leave_every_write_method_and_destination_unchanged() {
        for method in ["DELETE", "COPY", "MOVE"] {
            let directory = TestDir::new();
            let source = directory.path().join("source.txt");
            let destination = directory.path().join("destination.txt");
            fs::write(&source, b"source bytes").unwrap();
            fs::write(&destination, b"destination bytes").unwrap();
            let service =
                Service::new(crate::build_router(config(directory.path(), &["-w", "-d"])));
            let etag = format!("\"{}\"", path_metadata_and_etag(&source).unwrap().1);
            for (header, value) in [
                ("If-Match", "\"stale-tag\"".to_owned()),
                ("If-Match", format!("W/{etag}")),
                ("If-None-Match", "*".to_owned()),
                ("If-None-Match", format!("W/{etag}")),
                (
                    "If-Unmodified-Since",
                    "Thu, 01 Jan 1970 00:00:00 GMT".to_owned(),
                ),
            ] {
                let response = RequestBuilder::new(
                    "http://localhost/source.txt",
                    salvo::http::Method::from_bytes(method.as_bytes()).unwrap(),
                )
                .add_header("Destination", "/destination.txt", true)
                .add_header(header, value, true)
                .send(&service)
                .await;
                assert_eq!(
                    response.status_code,
                    Some(StatusCode::PRECONDITION_FAILED),
                    "{method} {header}"
                );
                assert_eq!(fs::read(&source).unwrap(), b"source bytes");
                assert_eq!(fs::read(&destination).unwrap(), b"destination bytes");
            }
            let response = RequestBuilder::new(
                "http://localhost/source.txt",
                salvo::http::Method::from_bytes(method.as_bytes()).unwrap(),
            )
            .add_header("Destination", "/destination.txt", true)
            .add_header("If-Match", etag, true)
            .add_header("If-Unmodified-Since", "Thu, 01 Jan 1970 00:00:00 GMT", true)
            .send(&service)
            .await;
            assert!(
                response.status_code.unwrap().is_success(),
                "matching If-Match must supersede the date: {method}"
            );
            assert_eq!(source.exists(), method == "COPY");
            if method != "DELETE" {
                assert_eq!(fs::read(&destination).unwrap(), b"source bytes");
            }
        }
    }

    #[tokio::test]
    async fn put_and_delete_share_the_version_check_and_mutation_boundary() {
        let directory = TestDir::new();
        let source = directory.path().join("source.txt");
        fs::write(&source, b"original").unwrap();
        let etag = format!("\"{}\"", path_metadata_and_etag(&source).unwrap().1);
        let service = Service::new(crate::build_router(config(directory.path(), &["-w"])));
        let put = TestClient::put("http://localhost/source.txt")
            .add_header("If-Match", &etag, true)
            .body("new longer bytes");
        let delete =
            RequestBuilder::new("http://localhost/source.txt", salvo::http::Method::DELETE)
                .add_header("If-Match", &etag, true);
        let (put, delete) = tokio::join!(put.send(&service), delete.send(&service));
        let statuses = [put.status_code.unwrap(), delete.status_code.unwrap()];
        assert_eq!(
            statuses.iter().filter(|status| status.is_success()).count(),
            1
        );
        assert_eq!(
            statuses
                .iter()
                .filter(|&&status| status == StatusCode::PRECONDITION_FAILED)
                .count(),
            1
        );
        if source.exists() {
            assert_eq!(fs::read(source).unwrap(), b"new longer bytes");
        }
    }

    #[tokio::test]
    async fn conditional_put_uses_the_same_date_policy() {
        let directory = TestDir::new();
        let source = directory.path().join("source.txt");
        fs::write(&source, b"original").unwrap();
        let service = Service::new(crate::build_router(config(directory.path(), &["-w"])));
        let response = TestClient::put("http://localhost/source.txt")
            .add_header("If-Unmodified-Since", "Thu, 01 Jan 1970 00:00:00 GMT", true)
            .body("replacement")
            .send(&service)
            .await;
        assert_eq!(response.status_code, Some(StatusCode::PRECONDITION_FAILED));
        assert_eq!(fs::read(source).unwrap(), b"original");
    }
}

#[cfg(test)]
mod control_header_tests {
    use salvo::test::RequestBuilder;

    use super::*;
    use crate::test_support::{TestDir, config};

    #[tokio::test]
    async fn valid_depth_values_are_ignored_for_non_collection_resources() {
        let directory = TestDir::new();
        let service = Service::new(crate::build_router(config(directory.path(), &["-w", "-d"])));
        for method in ["COPY", "MOVE"] {
            for depth in ["0", "1"] {
                fs::write(directory.path().join("source"), b"fixture").unwrap();
                let target = format!("/target-{method}-{depth}");
                let response =
                    RequestBuilder::new("http://localhost/source", method.parse().unwrap())
                        .add_header("Destination", target.clone(), true)
                        .add_header("Depth", depth, true)
                        .send(&service)
                        .await;
                assert_eq!(response.status_code, Some(StatusCode::CREATED));
                assert_eq!(
                    fs::read(directory.path().join(&target[1..])).unwrap(),
                    b"fixture"
                );
            }
        }
    }

    #[tokio::test]
    async fn malformed_headers_never_remove_or_replace_a_destination() {
        for method in ["COPY", "MOVE"] {
            let directory = TestDir::new();
            fs::create_dir_all(directory.path().join("source/nested")).unwrap();
            fs::write(directory.path().join("source/nested/file"), b"source").unwrap();
            fs::create_dir(directory.path().join("destination")).unwrap();
            let service =
                Service::new(crate::build_router(config(directory.path(), &["-w", "-d"])));
            for (name, value) in [
                ("Overwrite", "false"),
                ("Overwrite", "T, F"),
                ("Overwrite", ""),
                ("Depth", "garbage"),
                ("Depth", ""),
                ("Depth", "1"),
            ] {
                let response = RequestBuilder::new(
                    "http://localhost/source",
                    salvo::http::Method::from_bytes(method.as_bytes()).unwrap(),
                )
                .add_header("Destination", "/destination", true)
                .add_header(name, value, true)
                .send(&service)
                .await;
                assert_eq!(
                    response.status_code,
                    Some(StatusCode::BAD_REQUEST),
                    "{method} {name}:{value}"
                );
                assert_eq!(
                    fs::read(directory.path().join("source/nested/file")).unwrap(),
                    b"source"
                );
                assert!(directory.path().join("destination").is_dir());
                assert_eq!(
                    fs::read_dir(directory.path().join("destination"))
                        .unwrap()
                        .count(),
                    0
                );
            }
            for name in ["Overwrite", "Depth"] {
                let value = if name == "Overwrite" { "T" } else { "infinity" };
                let response = RequestBuilder::new(
                    "http://localhost/source",
                    salvo::http::Method::from_bytes(method.as_bytes()).unwrap(),
                )
                .add_header("Destination", "/destination", true)
                .add_header(name, value, true)
                .add_header(name, value, false)
                .send(&service)
                .await;
                assert_eq!(
                    response.status_code,
                    Some(StatusCode::BAD_REQUEST),
                    "{method}: repeated {name}"
                );
                assert!(directory.path().join("destination").is_dir());
            }
        }
    }

    #[tokio::test]
    async fn missing_control_headers_keep_the_documented_defaults() {
        for method in ["COPY", "MOVE"] {
            let directory = TestDir::new();
            fs::create_dir_all(directory.path().join("source/nested")).unwrap();
            fs::write(directory.path().join("source/nested/file"), b"source").unwrap();
            let service =
                Service::new(crate::build_router(config(directory.path(), &["-w", "-d"])));
            let response = RequestBuilder::new(
                "http://localhost/source",
                salvo::http::Method::from_bytes(method.as_bytes()).unwrap(),
            )
            .add_header("Destination", "/destination", true)
            .send(&service)
            .await;
            assert_eq!(response.status_code, Some(StatusCode::CREATED));
            assert_eq!(
                fs::read(directory.path().join("destination/nested/file")).unwrap(),
                b"source"
            );
            assert_eq!(directory.path().join("source").exists(), method == "COPY");
        }
    }
}
