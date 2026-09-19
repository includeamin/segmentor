//! Construction of the configurable CORS layer.
use axum::http::{HeaderName, HeaderValue, Method};
use tokio::time::Duration;
use tower_http::cors::{AllowHeaders, AllowMethods, AllowOrigin, CorsLayer, ExposeHeaders};

use crate::config::CorsConfig;
use crate::error::{Error, Result};

fn any(values: &[String]) -> bool {
    values.iter().any(|value| value == "*")
}
fn invalid(field: &str, value: &str) -> Error {
    Error::Configuration(format!("cors.{field} entry `{value}` is not valid"))
}
fn header_names(field: &str, values: &[String]) -> Result<Vec<HeaderName>> {
    values
        .iter()
        .map(|value| HeaderName::from_bytes(value.as_bytes()).map_err(|_| invalid(field, value)))
        .collect()
}

/// Builds the configured CORS layer, or `None` when a fronting proxy owns CORS.
pub(crate) fn cors_layer(config: &CorsConfig) -> Result<Option<CorsLayer>> {
    if !config.enabled {
        return Ok(None);
    }
    let origins = if any(&config.allowed_origins) {
        AllowOrigin::any()
    } else {
        AllowOrigin::list(
            config
                .allowed_origins
                .iter()
                .map(|origin| {
                    HeaderValue::from_str(origin).map_err(|_| invalid("allowed_origins", origin))
                })
                .collect::<Result<Vec<_>>>()?,
        )
    };
    let methods = if any(&config.allowed_methods) {
        AllowMethods::any()
    } else {
        AllowMethods::list(
            config
                .allowed_methods
                .iter()
                .map(|method| {
                    Method::from_bytes(method.to_ascii_uppercase().as_bytes())
                        .map_err(|_| invalid("allowed_methods", method))
                })
                .collect::<Result<Vec<_>>>()?,
        )
    };
    let allowed_headers = if any(&config.allowed_headers) {
        AllowHeaders::any()
    } else {
        AllowHeaders::list(header_names("allowed_headers", &config.allowed_headers)?)
    };
    let exposed_headers = if any(&config.exposed_headers) {
        ExposeHeaders::any()
    } else {
        ExposeHeaders::list(header_names("exposed_headers", &config.exposed_headers)?)
    };
    let mut layer = CorsLayer::new()
        .allow_origin(origins)
        .allow_methods(methods)
        .allow_headers(allowed_headers)
        .expose_headers(exposed_headers)
        .max_age(Duration::from_secs(config.max_age_seconds));
    if config.allow_credentials {
        layer = layer.allow_credentials(true);
    }
    Ok(Some(layer))
}
