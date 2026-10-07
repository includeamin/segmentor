//! The playback token check (TDD 0007, Mode 1), run on the HLS and DASH routes only.
//!
//! It runs before the asset is resolved, so a refused request never causes a resolver lookup and a
//! caller learns nothing about whether the asset exists: the ordering itself prevents the leak, with
//! no special-cased messages. A request is refused with `401` when it has no usable credential and
//! `403` when a valid token does not cover what it asked for.

use axum::extract::{Request, State};
use axum::http::header::{AUTHORIZATION, CACHE_CONTROL, COOKIE, WWW_AUTHENTICATE};
use axum::http::{HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use super::state::AppState;
use crate::authorization::{Denial, Resource};
use crate::config::AuthorizationSettings;
use crate::observability::metrics::AuthorizationOutcome;

/// A token that arrived as a query parameter. The playlists a request for it returns carry it into
/// every URL they list, so a player that only follows those URLs stays authorized.
#[derive(Debug, Clone)]
pub(crate) struct QueryToken {
    pub(crate) parameter: String,
    pub(crate) token: String,
}

/// The token a request carries: the first of the enabled transports that has one. There is no
/// falling back to another transport when it is bad, so what is checked is what was sent first.
fn find_token(settings: &AuthorizationSettings, request: &Request) -> Option<(String, bool)> {
    let headers = request.headers();
    if settings.transports.header
        && let Some(value) = headers
            .get(AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
        && let Some((scheme, token)) = value.split_once(' ')
        && scheme.eq_ignore_ascii_case("bearer")
    {
        return Some((token.trim().to_owned(), false));
    }
    if settings.transports.cookie {
        for line in headers.get_all(COOKIE) {
            let Ok(line) = line.to_str() else { continue };
            for pair in line.split(';') {
                if let Some((name, value)) = pair.trim().split_once('=')
                    && name == settings.cookie_name
                {
                    return Some((value.trim().to_owned(), false));
                }
            }
        }
    }
    if settings.transports.query
        && let Some(query) = request.uri().query()
    {
        for pair in query.split('&') {
            if let Some((name, value)) = pair.split_once('=')
                && name == settings.query_parameter
            {
                return Some((value.to_owned(), true));
            }
        }
    }
    None
}

fn refuse(status: StatusCode, message: &'static str) -> Response {
    let mut response = (status, message).into_response();
    let headers = response.headers_mut();
    headers.insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    if status == StatusCode::UNAUTHORIZED {
        headers.insert(WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
    }
    response
}

pub(crate) async fn authorize(
    State(state): State<AppState>,
    mut request: Request,
    next: Next,
) -> Response {
    let Some(settings) = state.authorization.clone() else {
        return next.run(request).await;
    };
    let outcome = |outcome: AuthorizationOutcome| state.metrics.authorization_check(outcome);
    let denied = |denial: Denial, request: &Request| {
        let (status, outcome_kind) = match denial {
            Denial::NotCovered => (StatusCode::FORBIDDEN, AuthorizationOutcome::Denied),
            Denial::BadSignature => (StatusCode::UNAUTHORIZED, AuthorizationOutcome::Denied),
            Denial::Expired | Denial::NotYetValid => {
                (StatusCode::UNAUTHORIZED, AuthorizationOutcome::Expired)
            }
            Denial::Malformed | Denial::TooLarge | Denial::WrongAlgorithm => {
                (StatusCode::UNAUTHORIZED, AuthorizationOutcome::Malformed)
            }
        };
        outcome(outcome_kind);
        // Normal, high-volume noise: the reason, never the token.
        tracing::debug!(
            event = "authorization_denied",
            reason = ?denial,
            http.path = %request.uri().path(),
        );
        refuse(status, denial.message())
    };

    let Some((token, from_query)) = find_token(&settings, &request) else {
        return denied(Denial::Malformed, &request);
    };
    let grant = match settings
        .verifier
        .verify(&token, std::time::SystemTime::now())
    {
        Ok(grant) => grant,
        Err(denial) => return denied(denial, &request),
    };
    let covered =
        Resource::from_path(request.uri().path()).is_some_and(|resource| grant.covers(&resource));
    if !covered {
        return denied(Denial::NotCovered, &request);
    }
    outcome(AuthorizationOutcome::Granted);
    if from_query {
        request.extensions_mut().insert(QueryToken {
            parameter: settings.query_parameter.clone(),
            token,
        });
    }
    next.run(request).await
}
