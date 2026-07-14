use std::sync::Arc;

use salvo::prelude::*;

use crate::config::AppConfig;
use crate::util::USER_AGENT;

#[handler]
pub async fn handle_trace(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let config = depot.get_typed::<Arc<AppConfig>>().unwrap().clone();

    if let Some(resp) = crate::hoops::auth::check_auth(req, &config) {
        *res = resp;
        return;
    }

    let remote = req.remote_addr().to_string();
    let url_path = req.uri().path().to_string();
    crate::config::log_msg(
        config.log,
        &format!("{} requested TRACE for {}", remote, url_path),
    );

    // Echo back headers as message/http
    let mut header_str = String::new();
    for (name, value) in req.headers() {
        if let Ok(v) = value.to_str() {
            header_str.push_str(&format!("{}: {}\r\n", name, v));
        }
    }

    res.status_code(StatusCode::OK);
    res.headers_mut().insert(
        salvo::http::header::CONTENT_TYPE,
        "message/http".parse().unwrap(),
    );
    res.headers_mut()
        .insert(salvo::http::header::SERVER, USER_AGENT.parse().unwrap());
    res.render(Text::Plain(header_str));
}
