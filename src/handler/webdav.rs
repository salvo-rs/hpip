use std::fs::{self, Metadata};
use std::io::{Error as IoError, ErrorKind as IoErrorKind, Write};
use std::mem;
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

    let depth = req
        .headers()
        .get("Depth")
        .and_then(|v| v.to_str().ok())
        .and_then(Depth::parse)
        .unwrap_or(Depth::Zero);

    let body_bytes = req
        .payload()
        .await
        .ok()
        .map(|b| b.to_vec())
        .unwrap_or_default();
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

    let url = req.uri().to_string();
    let ua = req
        .headers()
        .get(salvo::http::header::USER_AGENT)
        .and_then(|v| v.to_str().ok());

    match write_propfind_output(&config, &url, &req_p, &props, ua, depth) {
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

fn write_propfind_output(
    config: &AppConfig,
    url: &str,
    path: &Path,
    variant: &PropfindVariant,
    ua: Option<&str>,
    depth: Depth,
) -> Result<Vec<u8>, String> {
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

    let mut out = initialise_xml_output().map_err(|e| e.to_string())?;

    // Build namespace-aware start element
    let mut start = XmlWEvent::start_element("D:multistatus")
        .ns(WEBDAV_XML_NAMESPACES[0].0, WEBDAV_XML_NAMESPACES[0].1);
    for &(prefix, namespace) in &WEBDAV_XML_NAMESPACES[1..] {
        start = start.ns(prefix, namespace);
    }
    out.write(start).map_err(|e| e.to_string())?;

    let meta = path.metadata().map_err(|e| e.to_string())?;

    if let PropfindVariant::Props(custom) = variant {
        write_propfind_response_custom(config, &mut out, url, path, &meta, custom, just_names)
            .map_err(|e| e.to_string())?;
    } else {
        write_propfind_response(config, &mut out, url, path, &meta, props, just_names)
            .map_err(|e| e.to_string())?;
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
            )
            .map_err(|e| e.to_string())?;
        } else {
            write_propfind_recursive(
                config,
                &mut out,
                &mut url_owned,
                path,
                props,
                just_names,
                depth,
            )
            .map_err(|e| e.to_string())?;
        }
    }

    out.write(XmlWEvent::end_element())
        .map_err(|e| e.to_string())?;
    Ok(out.into_inner())
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
    out.write(XmlWEvent::characters(&escape_specials(url)))?;
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
    out.write(XmlWEvent::characters(&escape_specials(url)))?;
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
        let entries: Vec<_> = root_path
            .read_dir()
            .into_iter()
            .flatten()
            .flatten()
            .collect();
        for f in entries {
            root_url.truncate(root_url_orig_len);
            root_url.push_str(&f.file_name().to_string_lossy());

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
        let entries: Vec<_> = root_path
            .read_dir()
            .into_iter()
            .flatten()
            .flatten()
            .collect();
        for f in entries {
            root_url.truncate(root_url_orig_len);
            root_url.push_str(&f.file_name().to_string_lossy());

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
    let mut out = XmlWriter::new_with_config(vec![], default_xml_emitter_config());
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

    let body_bytes = req
        .payload()
        .await
        .ok()
        .map(|b| b.to_vec())
        .unwrap_or_default();
    let (props, actionables) = match parse_proppatch(&body_bytes) {
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

    set_times(
        &req_p,
        actionables.Win32LastModifiedTime,
        actionables.Win32LastAccessTime,
        actionables.Win32CreationTime,
    );
    if let Some(ex) = actionables.executable {
        set_executable(&req_p, ex);
    }

    let url = req.uri().to_string();
    match write_proppatch_output(&props, &url) {
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

fn parse_proppatch(
    body: &[u8],
) -> Result<(Vec<(OwnedXmlName, String)>, ProppatchActionables), String> {
    #[derive(Debug, Copy, Clone, PartialEq, Eq)]
    enum State {
        Start,
        PropertyUpdate,
        Action,
        Prop,
        InProp,
    }

    let mut xml = XmlReader::new_with_config(body, default_xml_parser_config());
    let mut state = State::Start;
    let mut props = vec![];
    let mut propname = None;
    let mut is_remove = false;
    let mut actionables = ProppatchActionables::default();
    let mut propdata = String::new();

    loop {
        let event = xml.next().map_err(|e| e.to_string())?;

        match (state, event) {
            (State::Start, XmlREvent::StartDocument { .. }) => (),
            (State::Start, XmlREvent::StartElement { ref name, .. })
                if name.local_name == "propertyupdate" =>
            {
                state = State::PropertyUpdate
            }

            (State::PropertyUpdate, XmlREvent::StartElement { ref name, .. })
                if name.local_name == "set" =>
            {
                state = State::Action;
                is_remove = false;
            }
            (State::PropertyUpdate, XmlREvent::StartElement { ref name, .. })
                if name.local_name == "remove" =>
            {
                state = State::Action;
                is_remove = true;
            }
            (State::PropertyUpdate, XmlREvent::EndElement { .. }) => {
                return Ok((props, actionables));
            }

            (State::Action, XmlREvent::StartElement { ref name, .. })
                if name.local_name == "prop" =>
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
                    props.push((name, mem::take(&mut propdata)));
                    state = State::Prop;
                }
            }
            (State::InProp, XmlREvent::Characters(data)) if !is_remove => {
                propdata = data;
                match propname.as_ref().map(|n| n.local_name.as_str()) {
                    Some("Win32CreationTime") => {
                        actionables.Win32CreationTime = win32time(&propdata)
                    }
                    Some("Win32LastAccessTime") => {
                        actionables.Win32LastAccessTime = win32time(&propdata)
                    }
                    Some("Win32LastModifiedTime") => {
                        actionables.Win32LastModifiedTime = win32time(&propdata)
                    }
                    Some("executable") => actionables.executable = Some(propdata == "T"),
                    _ => propdata.clear(),
                }
            }
            (State::InProp, _) => {}

            (_, ev) => return Err(format!("Unexpected event {:?}", ev)),
        }
    }
}

fn write_proppatch_output(
    props: &[(OwnedXmlName, String)],
    req_url: &str,
) -> Result<Vec<u8>, String> {
    let mut out = initialise_xml_output().map_err(|e| e.to_string())?;

    let mut start = XmlWEvent::start_element("D:multistatus")
        .ns(WEBDAV_XML_NAMESPACES[0].0, WEBDAV_XML_NAMESPACES[0].1);
    for &(prefix, namespace) in &WEBDAV_XML_NAMESPACES[1..] {
        start = start.ns(prefix, namespace);
    }
    out.write(start).map_err(|e| e.to_string())?;

    out.write(XmlWEvent::start_element("D:href"))
        .map_err(|e| e.to_string())?;
    out.write(XmlWEvent::characters(req_url))
        .map_err(|e| e.to_string())?;
    out.write(XmlWEvent::end_element())
        .map_err(|e| e.to_string())?;

    out.write(XmlWEvent::start_element("D:propstat"))
        .map_err(|e| e.to_string())?;

    for (name, _) in props {
        out.write(XmlWEvent::start_element("D:prop"))
            .map_err(|e| e.to_string())?;
        write_start_prop_element(&mut out, name.borrow()).map_err(|e| e.to_string())?;
        out.write(XmlWEvent::end_element())
            .map_err(|e| e.to_string())?;
        out.write(XmlWEvent::end_element())
            .map_err(|e| e.to_string())?;
    }

    out.write(XmlWEvent::start_element("D:status"))
        .map_err(|e| e.to_string())?;
    out.write(XmlWEvent::characters("HTTP/1.1 409 Conflict"))
        .map_err(|e| e.to_string())?;
    out.write(XmlWEvent::end_element())
        .map_err(|e| e.to_string())?;

    out.write(XmlWEvent::end_element())
        .map_err(|e| e.to_string())?; // propstat
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
    let body = req
        .payload()
        .await
        .ok()
        .map(|b| b.to_vec())
        .unwrap_or_default();
    if !body.is_empty() {
        res.status_code(StatusCode::UNSUPPORTED_MEDIA_TYPE);
        return;
    }

    match fs::create_dir(&req_p) {
        Ok(()) => {
            res.status_code(StatusCode::CREATED);
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
    let config = depot.get_typed::<Arc<AppConfig>>().unwrap().clone();
    let url_path_raw = req.uri().path().to_string();
    let segments: Vec<&str> = url_path_raw.split('/').filter(|s| !s.is_empty()).collect();
    let (req_p, ..) = resolve_path(
        &config.hosted_directory.1,
        &segments,
        config.follow_symlinks,
    );

    let req_p_clone = req_p.clone();
    let is_file = req_p
        .metadata()
        .map(|m| is_actually_file(&m.file_type(), &req_p))
        .unwrap_or(true);

    handle_copy_move(req, depot, res, true).await;

    // If copy_move succeeded (Created or NoContent), remove source
    if res.status_code == Some(StatusCode::CREATED)
        || res.status_code == Some(StatusCode::NO_CONTENT)
    {
        let removal = if is_file {
            fs::remove_file(&req_p_clone)
        } else {
            fs::remove_dir_all(&req_p_clone)
        };
        if removal.is_err() {
            res.status_code(StatusCode::LOCKED);
            res.body(salvo::http::ResBody::None);
        }
    }
}

async fn handle_copy_move(req: &mut Request, depot: &mut Depot, res: &mut Response, is_move: bool) {
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

    // Extract path from destination URL
    let dest_path_str = if dest_url.starts_with("http://") || dest_url.starts_with("https://") {
        // Parse as full URL, extract path portion
        match dest_url.find("://") {
            Some(idx) => {
                let after_scheme = &dest_url[idx + 3..];
                match after_scheme.find('/') {
                    Some(slash_idx) => after_scheme[slash_idx..].to_string(),
                    None => "/".to_string(),
                }
            }
            None => dest_url.clone(),
        }
    } else if dest_url.starts_with('/') {
        dest_url.clone()
    } else {
        set_error(
            res,
            StatusCode::BAD_REQUEST,
            "400 Bad Request",
            "Destination URL invalid.",
        );
        return;
    };
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

    let depth = req
        .headers()
        .get("Depth")
        .and_then(|v| v.to_str().ok())
        .and_then(Depth::parse)
        .unwrap_or(Depth::Infinity);

    let overwrite = req
        .headers()
        .get("Overwrite")
        .and_then(|v| v.to_str().ok())
        .and_then(Overwrite::parse)
        .unwrap_or_default()
        .0;

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
    if dest_p.exists() {
        if !overwrite {
            res.status_code(StatusCode::PRECONDITION_FAILED);
            return;
        }
        if !is_actually_file(&dest_p.metadata().unwrap().file_type(), &dest_p)
            && fs::remove_dir(&dest_p).is_err()
        {
            res.status_code(StatusCode::LOCKED);
            return;
        }
        overwritten = true;
    }

    let source_file = is_actually_file(&req_p.metadata().unwrap().file_type(), &req_p);
    if source_file {
        copy_response(res, fs::copy(&req_p, &dest_p).map(|_| ()), overwritten);
    } else {
        match depth {
            Depth::Zero if !is_move => copy_response(res, fs::create_dir(&dest_p), overwritten),
            Depth::Infinity => match copy_dir(&req_p, &dest_p) {
                Ok(errors) => {
                    if errors.is_empty() {
                        copy_response(res, Ok(()), overwritten);
                    } else {
                        res.status_code(StatusCode::MULTI_STATUS);
                        if let Ok(xml_bytes) = copy_response_multierror(&errors, &url_path_raw) {
                            res.headers_mut().insert(
                                salvo::http::header::CONTENT_TYPE,
                                "text/xml; charset=utf-8".parse().unwrap(),
                            );
                            res.write_body(xml_bytes).ok();
                        }
                    }
                }
                Err(_) => copy_response(res, Err(IoError::other("copy failed")), overwritten),
            },
            _ => {
                set_error(
                    res,
                    StatusCode::BAD_REQUEST,
                    "400 Bad Request",
                    &format!("Invalid depth: {}", depth),
                );
            }
        }
    }
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
        Err(_) => {
            res.status_code(StatusCode::INSUFFICIENT_STORAGE);
        }
    }
}

fn copy_response_multierror(
    errors: &[(IoError, String)],
    base_url: &str,
) -> Result<Vec<u8>, String> {
    let mut out = initialise_xml_output().map_err(|e| e.to_string())?;
    out.write(
        XmlWEvent::start_element("D:multistatus")
            .ns(WEBDAV_XML_NAMESPACE_DAV.0, WEBDAV_XML_NAMESPACE_DAV.1),
    )
    .map_err(|e| e.to_string())?;
    out.write(XmlWEvent::start_element("D:response"))
        .map_err(|e| e.to_string())?;

    for (_, subp) in errors {
        out.write(XmlWEvent::start_element("D:href"))
            .map_err(|e| e.to_string())?;
        let href = format!("{}/{}", base_url.trim_end_matches('/'), subp);
        out.write(XmlWEvent::characters(&href))
            .map_err(|e| e.to_string())?;
        out.write(XmlWEvent::end_element())
            .map_err(|e| e.to_string())?;
    }

    out.write(XmlWEvent::start_element("D:status"))
        .map_err(|e| e.to_string())?;
    out.write(XmlWEvent::characters("HTTP/1.1 507 Insufficient Storage"))
        .map_err(|e| e.to_string())?;
    out.write(XmlWEvent::end_element())
        .map_err(|e| e.to_string())?;
    out.write(XmlWEvent::end_element())
        .map_err(|e| e.to_string())?;
    out.write(XmlWEvent::end_element())
        .map_err(|e| e.to_string())?;

    Ok(out.into_inner())
}

// ─── Helpers ───────────────────────────────────────────

fn set_error(res: &mut Response, status: StatusCode, title: &str, msg: &str) {
    let body = error_html(title, msg, "");
    res.status_code(status);
    res.headers_mut().insert(
        salvo::http::header::CONTENT_TYPE,
        "text/html; charset=utf-8".parse().unwrap(),
    );
    res.headers_mut()
        .insert(salvo::http::header::SERVER, USER_AGENT.parse().unwrap());
    res.render(Text::Html(body));
}
