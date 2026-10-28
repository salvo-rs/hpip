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

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use salvo::test::RequestBuilder;

    use super::*;
    use crate::test_support::{TestDir, config};

    #[tokio::test]
    async fn options_advertises_exactly_the_configured_capabilities_once() {
        let directory = TestDir::new();
        for (flags, additional) in [
            (vec![], vec![]),
            (vec!["-A"], vec!["POST"]),
            (vec!["-w"], vec!["PUT", "DELETE"]),
            (vec!["-d"], vec!["PROPFIND"]),
            (vec!["-D"], vec![]),
            (vec!["-w", "-D"], vec!["PUT", "DELETE", "MKCOL", "MOVE"]),
            (
                vec!["-w", "-d"],
                vec![
                    "PUT",
                    "DELETE",
                    "PROPFIND",
                    "PROPPATCH",
                    "MKCOL",
                    "COPY",
                    "MOVE",
                ],
            ),
            (vec!["-A", "-d"], vec!["POST", "PROPFIND"]),
            (
                vec!["-A", "-w", "-d"],
                vec![
                    "POST",
                    "PUT",
                    "DELETE",
                    "PROPFIND",
                    "PROPPATCH",
                    "MKCOL",
                    "COPY",
                    "MOVE",
                ],
            ),
        ] {
            let service = Service::new(crate::build_router(config(directory.path(), &flags)));
            let response = RequestBuilder::new("http://localhost/", salvo::http::Method::OPTIONS)
                .send(&service)
                .await;
            assert_eq!(response.status_code, Some(StatusCode::NO_CONTENT));
            let methods: Vec<_> = response.headers()["allow"]
                .to_str()
                .unwrap()
                .split(',')
                .map(str::trim)
                .collect();
            let unique: BTreeSet<_> = methods.iter().copied().collect();
            assert_eq!(methods.len(), unique.len(), "duplicate with {flags:?}");
            let expected: BTreeSet<_> = ["OPTIONS", "GET", "HEAD"]
                .into_iter()
                .chain(additional)
                .collect();
            assert_eq!(unique, expected, "{flags:?}");
        }
    }
}
