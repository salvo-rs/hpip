use std::sync::Arc;

use salvo::prelude::*;

use crate::config::AppConfig;
use crate::util::USER_AGENT;

#[handler]
pub async fn handle_options(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let config = depot.get_typed::<Arc<AppConfig>>().unwrap().clone();

    if let Some(resp) = crate::hoops::auth::check_auth(req, &config) {
        *res = resp;
        return;
    }

    let remote = req.remote_addr().to_string();
    crate::config::log_msg(config.log, &format!("{} asked for OPTIONS", remote));

    res.status_code(StatusCode::NO_CONTENT);
    res.headers_mut()
        .insert(salvo::http::header::SERVER, USER_AGENT.parse().unwrap());
    res.headers_mut().insert(
        salvo::http::header::ALLOW,
        config.allowed_methods.join(", ").parse().unwrap(),
    );
}
