use std::convert::Infallible;
use std::net::{IpAddr, SocketAddr};
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use crate::ferron_res::server_software::SERVER_SOFTWARE;
use crate::ferron_util::combine_config::combine_config;
use crate::ferron_util::error_pages::generate_default_error_page;
use crate::ferron_util::url_sanitizer::sanitize_url;

use async_channel::Sender;
use chrono::prelude::*;
use ferron_common::{
  ErrorLogger, LogMessage, RequestData, ServerConfigRoot, ServerModuleHandlers, SocketData,
};
use futures_util::TryStreamExt;
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Empty, Full, StreamBody};
use hyper::body::{Body, Bytes, Frame};
use hyper::header::{self, HeaderName, HeaderValue};
use hyper::{HeaderMap, Method, Request, Response, StatusCode};
use hyper_tungstenite::is_upgrade_request;
use tokio::fs;
use tokio::io::BufReader;
use tokio::time::timeout;
use tokio_util::io::ReaderStream;
use yaml_rust2::Yaml;

async fn generate_error_response(
  status_code: StatusCode,
  config: &ServerConfigRoot,
  headers: &Option<HeaderMap>,
) -> Response<BoxBody<Bytes, std::io::Error>> {
  let bare_body =
    generate_default_error_page(status_code, config.get("serverAdministratorEmail").as_str());
  let mut content_length: Option<u64> = match bare_body.len().try_into() {
    Ok(content_length) => Some(content_length),
    Err(_) => None,
  };
  let mut response_body = Full::new(Bytes::from(bare_body))
    .map_err(|e| match e {})
    .boxed();

  if let Some(error_pages) = config.get("errorPages").as_vec() {
    for error_page_yaml in error_pages {
      if let Some(page_status_code) = error_page_yaml["scode"].as_i64() {
        let page_status_code = match StatusCode::from_u16(match page_status_code.try_into() {
          Ok(status_code) => status_code,
          Err(_) => continue,
        }) {
          Ok(status_code) => status_code,
          Err(_) => continue,
        };
        if status_code != page_status_code {
          continue;
        }
        if let Some(page_path) = error_page_yaml["path"].as_str() {
          let file = fs::File::open(page_path).await;

          let file = match file {
            Ok(file) => file,
            Err(_) => continue,
          };

          content_length = match file.metadata().await {
            Ok(metadata) => Some(metadata.len()),
            Err(_) => None,
          };

          // Use BufReader for better performance.
          let reader_stream = ReaderStream::new(BufReader::with_capacity(12800, file));

          let stream_body = StreamBody::new(reader_stream.map_ok(Frame::data));
          let boxed_body = stream_body.boxed();

          response_body = boxed_body;

          break;
        }
      }
    }
  }

  let mut response_builder = Response::builder().status(status_code);

  if let Some(headers) = headers {
    let headers_iter = headers.iter();
    for (name, value) in headers_iter {
      if name != header::CONTENT_TYPE && name != header::CONTENT_LENGTH {
        response_builder = response_builder.header(name, value);
      }
    }
  }

  if let Some(content_length) = content_length {
    response_builder = response_builder.header(header::CONTENT_LENGTH, content_length);
  }
  response_builder = response_builder.header(header::CONTENT_TYPE, "text/html");

  response_builder.body(response_body).unwrap_or_default()
}

#[allow(clippy::too_many_arguments)]
async fn log_combined(
  logger: &Sender<LogMessage>,
  client_ip: IpAddr,
  auth_user: Option<String>,
  method: String,
  request_path: String,
  protocol: String,
  status_code: u16,
  content_length: Option<u64>,
  referrer: Option<String>,
  user_agent: Option<String>,
) {
  let now: DateTime<Local> = Local::now();
  let formatted_time = now.format("%d/%b/%Y:%H:%M:%S %z").to_string();
  logger
    .send(LogMessage::new(
      format!(
        "{} - {} [{}] \"{} {} {}\" {} {} {} {}",
        client_ip,
        match auth_user {
          Some(auth_user) => auth_user,
          None => String::from("-"),
        },
        formatted_time,
        method,
        request_path,
        protocol,
        status_code,
        match content_length {
          Some(content_length) => format!("{}", content_length),
          None => String::from("-"),
        },
        match referrer {
          Some(referrer) => format!(
            "\"{}\"",
            referrer.replace("\\", "\\\\").replace("\"", "\\\"")
          ),
          None => String::from("-"),
        },
        match user_agent {
          Some(user_agent) => format!(
            "\"{}\"",
            user_agent.replace("\\", "\\\\").replace("\"", "\\\"")
          ),
          None => String::from("-"),
        },
      ),
      false,
    ))
    .await
    .unwrap_or_default();
}

#[allow(clippy::too_many_arguments)]
async fn request_handler_wrapped(
  mut request: Request<BoxBody<Bytes, hyper::Error>>,
  remote_address: SocketAddr,
  local_address: SocketAddr,
  encrypted: bool,
  global_config_root: Arc<ServerConfigRoot>,
  host_config: Arc<Yaml>,
  logger: Sender<LogMessage>,
  handlers_vec: Vec<Box<dyn ServerModuleHandlers + Send>>,
) -> Result<Response<BoxBody<Bytes, std::io::Error>>, Infallible> {
  let is_proxy_request = match request.version() {
    hyper::Version::HTTP_2 | hyper::Version::HTTP_3 => {
      request.method() == hyper::Method::CONNECT && request.uri().host().is_some()
    }
    _ => request.uri().host().is_some(),
  };
  let is_connect_proxy_request = request.method() == hyper::Method::CONNECT;

  // Collect request data for logging
  let log_method = String::from(request.method().as_str());
  let log_request_path = match is_proxy_request {
    true => request.uri().to_string(),
    false => format!(
      "{}{}",
      request.uri().path(),
      match request.uri().query() {
        Some(query) => format!("?{}", query),
        None => String::from(""),
      }
    ),
  };
  let log_protocol = String::from(match request.version() {
    hyper::Version::HTTP_09 => "HTTP/0.9",
    hyper::Version::HTTP_10 => "HTTP/1.0",
    hyper::Version::HTTP_11 => "HTTP/1.1",
    hyper::Version::HTTP_2 => "HTTP/2.0",
    hyper::Version::HTTP_3 => "HTTP/3.0",
    _ => "HTTP/Unknown",
  });
  let log_referrer = match request.headers().get(header::REFERER) {
    Some(header_value) => match header_value.to_str() {
      Ok(header_value) => Some(String::from(header_value)),
      Err(_) => None,
    },
    None => None,
  };
  let log_user_agent = match request.headers().get(header::USER_AGENT) {
    Some(header_value) => match header_value.to_str() {
      Ok(header_value) => Some(String::from(header_value)),
      Err(_) => None,
    },
    None => None,
  };
  let log_enabled = global_config_root.get("logFilePath").as_str().is_some();
  let error_log_enabled = global_config_root
    .get("errorLogFilePath")
    .as_str()
    .is_some();

  // Construct SocketData
  let mut socket_data = SocketData::new(remote_address, local_address, encrypted);

  let host_header_option = request.headers().get(header::HOST);
  if let Some(header_data) = host_header_option {
    match header_data.to_str() {
      Ok(host_header) => {
        let host_header_lower_case = host_header.to_lowercase();
        if host_header_lower_case != *host_header {
          let host_header_value = match HeaderValue::from_str(&host_header_lower_case) {
            Ok(host_header_value) => host_header_value,
            Err(err) => {
              if error_log_enabled {
                logger
                  .send(LogMessage::new(
                    format!("Host header sanitation error: {}", err),
                    true,
                  ))
                  .await
                  .unwrap_or_default();
              }
              let response = Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .header(header::CONTENT_TYPE, "text/html")
                .body(
                  Full::new(Bytes::from(generate_default_error_page(
                    StatusCode::BAD_REQUEST,
                    None,
                  )))
                  .map_err(|e| match e {})
                  .boxed(),
                )
                .unwrap_or_default();

              if log_enabled {
                log_combined(
                  &logger,
                  socket_data.remote_addr.ip(),
                  None,
                  log_method,
                  log_request_path,
                  log_protocol,
                  response.status().as_u16(),
                  match response.headers().get(header::CONTENT_LENGTH) {
                    Some(header_value) => match header_value.to_str() {
                      Ok(header_value) => match header_value.parse::<u64>() {
                        Ok(content_length) => Some(content_length),
                        Err(_) => response.body().size_hint().exact(),
                      },
                      Err(_) => response.body().size_hint().exact(),
                    },
                    None => response.body().size_hint().exact(),
                  },
                  log_referrer,
                  log_user_agent,
                )
                .await;
              }
              let (mut response_parts, response_body) = response.into_parts();
              if let Ok(server_string) = HeaderValue::from_str(SERVER_SOFTWARE) {
                response_parts.headers.insert(header::SERVER, server_string);
              };
              return Ok(Response::from_parts(response_parts, response_body));
            }
          };

          request
            .headers_mut()
            .insert(header::HOST, host_header_value);
        }
      }
      Err(err) => {
        if error_log_enabled {
          logger
            .send(LogMessage::new(
              format!("Host header sanitation error: {}", err),
              true,
            ))
            .await
            .unwrap_or_default();
        }
        let response = Response::builder()
          .status(StatusCode::BAD_REQUEST)
          .header(header::CONTENT_TYPE, "text/html")
          .body(
            Full::new(Bytes::from(generate_default_error_page(
              StatusCode::BAD_REQUEST,
              None,
            )))
            .map_err(|e| match e {})
            .boxed(),
          )
          .unwrap_or_default();
        if log_enabled {
          log_combined(
            &logger,
            socket_data.remote_addr.ip(),
            None,
            log_method,
            log_request_path,
            log_protocol,
            response.status().as_u16(),
            match response.headers().get(header::CONTENT_LENGTH) {
              Some(header_value) => match header_value.to_str() {
                Ok(header_value) => match header_value.parse::<u64>() {
                  Ok(content_length) => Some(content_length),
                  Err(_) => response.body().size_hint().exact(),
                },
                Err(_) => response.body().size_hint().exact(),
              },
              None => response.body().size_hint().exact(),
            },
            log_referrer,
            log_user_agent,
          )
          .await;
        }
        let (mut response_parts, response_body) = response.into_parts();
        if let Ok(server_string) = HeaderValue::from_str(SERVER_SOFTWARE) {
          response_parts.headers.insert(header::SERVER, server_string);
        };
        return Ok(Response::from_parts(response_parts, response_body));
      }
    }
  };

  // Combine the server configuration
  let combined_config = match combine_config(
    global_config_root,
    host_config,
    match is_proxy_request || is_connect_proxy_request {
      false => match request.headers().get(header::HOST) {
        Some(value) => match value.to_str() {
          Ok(value) => Some(value),
          Err(_) => None,
        },
        None => None,
      },
      true => None,
    },
    local_address.ip(),
    request.uri().path(),
  ) {
    Some(config) => config,
    None => {
      if error_log_enabled {
        logger
          .send(LogMessage::new(
            String::from("Cannot determine server configuration"),
            true,
          ))
          .await
          .unwrap_or_default();
      }
      let response = Response::builder()
        .status(StatusCode::INTERNAL_SERVER_ERROR)
        .header(header::CONTENT_TYPE, "text/html")
        .body(
          Full::new(Bytes::from(generate_default_error_page(
            StatusCode::INTERNAL_SERVER_ERROR,
            None,
          )))
          .map_err(|e| match e {})
          .boxed(),
        )
        .unwrap_or_default();
      if log_enabled {
        log_combined(
          &logger,
          socket_data.remote_addr.ip(),
          None,
          log_method,
          log_request_path,
          log_protocol,
          response.status().as_u16(),
          match response.headers().get(header::CONTENT_LENGTH) {
            Some(header_value) => match header_value.to_str() {
              Ok(header_value) => match header_value.parse::<u64>() {
                Ok(content_length) => Some(content_length),
                Err(_) => response.body().size_hint().exact(),
              },
              Err(_) => response.body().size_hint().exact(),
            },
            None => response.body().size_hint().exact(),
          },
          log_referrer,
          log_user_agent,
        )
        .await;
      }
      let (mut response_parts, response_body) = response.into_parts();
      if let Ok(server_string) = HeaderValue::from_str(SERVER_SOFTWARE) {
        response_parts.headers.insert(header::SERVER, server_string);
      };
      return Ok(Response::from_parts(response_parts, response_body));
    }
  };

  let url_pathname = request.uri().path();
  let sanitized_url_pathname = match sanitize_url(
    url_pathname,
    combined_config
      .get("allowDoubleSlashes")
      .as_bool()
      .unwrap_or_default(),
  ) {
    Ok(sanitized_url) => sanitized_url,
    Err(err) => {
      if error_log_enabled {
        logger
          .send(LogMessage::new(
            format!("URL sanitation error: {}", err),
            true,
          ))
          .await
          .unwrap_or_default();
      }
      let response =
        generate_error_response(StatusCode::BAD_REQUEST, &combined_config, &None).await;
      if log_enabled {
        log_combined(
          &logger,
          socket_data.remote_addr.ip(),
          None,
          log_method,
          log_request_path,
          log_protocol,
          response.status().as_u16(),
          match response.headers().get(header::CONTENT_LENGTH) {
            Some(header_value) => match header_value.to_str() {
              Ok(header_value) => match header_value.parse::<u64>() {
                Ok(content_length) => Some(content_length),
                Err(_) => response.body().size_hint().exact(),
              },
              Err(_) => response.body().size_hint().exact(),
            },
            None => response.body().size_hint().exact(),
          },
          log_referrer,
          log_user_agent,
        )
        .await;
      }
      let (mut response_parts, response_body) = response.into_parts();
      if let Some(custom_headers_hash) = combined_config.get("customHeaders").as_hash() {
        let custom_headers_hash_iter = custom_headers_hash.iter();
        for (header_name, header_value) in custom_headers_hash_iter {
          if let Some(header_name) = header_name.as_str() {
            if let Some(header_value) = header_value.as_str() {
              if !response_parts.headers.contains_key(header_name) {
                if let Ok(header_value) = HeaderValue::from_str(header_value) {
                  if let Ok(header_name) = HeaderName::from_str(header_name) {
                    response_parts.headers.insert(header_name, header_value);
                  }
                }
              }
            }
          }
        }
      }
      if let Ok(server_string) = HeaderValue::from_str(SERVER_SOFTWARE) {
        response_parts.headers.insert(header::SERVER, server_string);
      };
      return Ok(Response::from_parts(response_parts, response_body));
    }
  };

  if sanitized_url_pathname != url_pathname {
    let (mut parts, body) = request.into_parts();
    let mut url_parts = parts.uri.into_parts();
    url_parts.path_and_query = Some(
      match format!(
        "{}{}",
        sanitized_url_pathname,
        match url_parts.path_and_query {
          Some(path_and_query) => {
            match path_and_query.query() {
              Some(query) => format!("?{}", query),
              None => String::from(""),
            }
          }
          None => String::from(""),
        }
      )
      .parse()
      {
        Ok(path_and_query) => path_and_query,
        Err(err) => {
          if error_log_enabled {
            logger
              .send(LogMessage::new(
                format!("URL sanitation error: {}", err),
                true,
              ))
              .await
              .unwrap_or_default();
          }
          let response =
            generate_error_response(StatusCode::BAD_REQUEST, &combined_config, &None).await;
          if log_enabled {
            log_combined(
              &logger,
              socket_data.remote_addr.ip(),
              None,
              log_method,
              log_request_path,
              log_protocol,
              response.status().as_u16(),
              match response.headers().get(header::CONTENT_LENGTH) {
                Some(header_value) => match header_value.to_str() {
                  Ok(header_value) => match header_value.parse::<u64>() {
                    Ok(content_length) => Some(content_length),
                    Err(_) => response.body().size_hint().exact(),
                  },
                  Err(_) => response.body().size_hint().exact(),
                },
                None => response.body().size_hint().exact(),
              },
              log_referrer,
              log_user_agent,
            )
            .await;
          }
          let (mut response_parts, response_body) = response.into_parts();
          if let Some(custom_headers_hash) = combined_config.get("customHeaders").as_hash() {
            let custom_headers_hash_iter = custom_headers_hash.iter();
            for (header_name, header_value) in custom_headers_hash_iter {
              if let Some(header_name) = header_name.as_str() {
                if let Some(header_value) = header_value.as_str() {
                  if !response_parts.headers.contains_key(header_name) {
                    if let Ok(header_value) = HeaderValue::from_str(header_value) {
                      if let Ok(header_name) = HeaderName::from_str(header_name) {
                        response_parts.headers.insert(header_name, header_value);
                      }
                    }
                  }
                }
              }
            }
          }
          if let Ok(server_string) = HeaderValue::from_str(SERVER_SOFTWARE) {
            response_parts.headers.insert(header::SERVER, server_string);
          };
          return Ok(Response::from_parts(response_parts, response_body));
        }
      },
    );
    parts.uri = match hyper::Uri::from_parts(url_parts) {
      Ok(uri) => uri,
      Err(err) => {
        if error_log_enabled {
          logger
            .send(LogMessage::new(
              format!("URL sanitation error: {}", err),
              true,
            ))
            .await
            .unwrap_or_default();
        }
        let response =
          generate_error_response(StatusCode::BAD_REQUEST, &combined_config, &None).await;
        if log_enabled {
          log_combined(
            &logger,
            socket_data.remote_addr.ip(),
            None,
            log_method,
            log_request_path,
            log_protocol,
            response.status().as_u16(),
            match response.headers().get(header::CONTENT_LENGTH) {
              Some(header_value) => match header_value.to_str() {
                Ok(header_value) => match header_value.parse::<u64>() {
                  Ok(content_length) => Some(content_length),
                  Err(_) => response.body().size_hint().exact(),
                },
                Err(_) => response.body().size_hint().exact(),
              },
              None => response.body().size_hint().exact(),
            },
            log_referrer,
            log_user_agent,
          )
          .await;
        }
        let (mut response_parts, response_body) = response.into_parts();
        if let Some(custom_headers_hash) = combined_config.get("customHeaders").as_hash() {
          let custom_headers_hash_iter = custom_headers_hash.iter();
          for (header_name, header_value) in custom_headers_hash_iter {
            if let Some(header_name) = header_name.as_str() {
              if let Some(header_value) = header_value.as_str() {
                if !response_parts.headers.contains_key(header_name) {
                  if let Ok(header_value) = HeaderValue::from_str(header_value) {
                    if let Ok(header_name) = HeaderName::from_str(header_name) {
                      response_parts.headers.insert(header_name, header_value);
                    }
                  }
                }
              }
            }
          }
        }
        if let Ok(server_string) = HeaderValue::from_str(SERVER_SOFTWARE) {
          response_parts.headers.insert(header::SERVER, server_string);
        };
        return Ok(Response::from_parts(response_parts, response_body));
      }
    };
    request = Request::from_parts(parts, body);
  }

  if request.uri().path() == "*" {
    let response = match request.method() {
      &Method::OPTIONS => Response::builder()
        .status(StatusCode::NO_CONTENT)
        .header(header::ALLOW, "GET, POST, HEAD, OPTIONS")
        .body(Empty::new().map_err(|e| match e {}).boxed())
        .unwrap_or_default(),
      _ => {
        let mut header_map = HeaderMap::new();
        if let Ok(header_value) = HeaderValue::from_str("GET, POST, HEAD, OPTIONS") {
          header_map.insert(header::ALLOW, header_value);
        };
        generate_error_response(StatusCode::BAD_REQUEST, &combined_config, &Some(header_map)).await
      }
    };
    if log_enabled {
      log_combined(
        &logger,
        socket_data.remote_addr.ip(),
        None,
        log_method,
        log_request_path,
        log_protocol,
        response.status().as_u16(),
        match response.headers().get(header::CONTENT_LENGTH) {
          Some(header_value) => match header_value.to_str() {
            Ok(header_value) => match header_value.parse::<u64>() {
              Ok(content_length) => Some(content_length),
              Err(_) => response.body().size_hint().exact(),
            },
            Err(_) => response.body().size_hint().exact(),
          },
          None => response.body().size_hint().exact(),
        },
        log_referrer,
        log_user_agent,
      )
      .await;
    }
    let (mut response_parts, response_body) = response.into_parts();
    if let Some(custom_headers_hash) = combined_config.get("customHeaders").as_hash() {
      let custom_headers_hash_iter = custom_headers_hash.iter();
      for (header_name, header_value) in custom_headers_hash_iter {
        if let Some(header_name) = header_name.as_str() {
          if let Some(header_value) = header_value.as_str() {
            if !response_parts.headers.contains_key(header_name) {
              if let Ok(header_value) = HeaderValue::from_str(header_value) {
                if let Ok(header_name) = HeaderName::from_str(header_name) {
                  response_parts.headers.insert(header_name, header_value);
                }
              }
            }
          }
        }
      }
    }
    if let Ok(server_string) = HeaderValue::from_str(SERVER_SOFTWARE) {
      response_parts.headers.insert(header::SERVER, server_string);
    };
    return Ok(Response::from_parts(response_parts, response_body));
  }

  let cloned_logger = logger.clone();
  let error_logger = match error_log_enabled {
    true => ErrorLogger::new(cloned_logger),
    false => ErrorLogger::without_logger(),
  };

  if is_connect_proxy_request {
    let mut connect_proxy_handlers = None;
    for mut handlers in handlers_vec {
      if handlers.does_connect_proxy_requests() {
        connect_proxy_handlers = Some(handlers);
        break;
      }
    }

    if let Some(mut connect_proxy_handlers) = connect_proxy_handlers {
      if let Some(connect_address) = request.uri().authority().map(|auth| auth.to_string()) {
        // Variables moved to before "tokio::spawn" to avoid issues with moved values
        let client_ip = socket_data.remote_addr.ip();
        let custom_headers_yaml = combined_config.get("customHeaders");

        tokio::spawn(async move {
          match hyper::upgrade::on(request).await {
            Ok(upgraded_request) => {
              let result = connect_proxy_handlers
                .connect_proxy_request_handler(
                  upgraded_request,
                  &connect_address,
                  &combined_config,
                  &socket_data,
                  &error_logger,
                )
                .await;
              match result {
                Ok(_) => (),
                Err(err) => {
                  error_logger
                    .log(&format!("Unexpected error for CONNECT request: {}", err))
                    .await;
                }
              }
            }
            Err(err) => {
              error_logger
                .log(&format!(
                  "Error while upgrading HTTP CONNECT request: {}",
                  err
                ))
                .await
            }
          }
        });

        let response = Response::builder()
          .body(Empty::new().map_err(|e| match e {}).boxed())
          .unwrap_or_default();

        if log_enabled {
          log_combined(
            &logger,
            client_ip,
            None,
            log_method,
            log_request_path,
            log_protocol,
            response.status().as_u16(),
            match response.headers().get(header::CONTENT_LENGTH) {
              Some(header_value) => match header_value.to_str() {
                Ok(header_value) => match header_value.parse::<u64>() {
                  Ok(content_length) => Some(content_length),
                  Err(_) => response.body().size_hint().exact(),
                },
                Err(_) => response.body().size_hint().exact(),
              },
              None => response.body().size_hint().exact(),
            },
            log_referrer,
            log_user_agent,
          )
          .await;
        }

        let (mut response_parts, response_body) = response.into_parts();
        if let Some(custom_headers_hash) = custom_headers_yaml.as_hash() {
          let custom_headers_hash_iter = custom_headers_hash.iter();
          for (header_name, header_value) in custom_headers_hash_iter {
            if let Some(header_name) = header_name.as_str() {
              if let Some(header_value) = header_value.as_str() {
                if !response_parts.headers.contains_key(header_name) {
                  if let Ok(header_value) = HeaderValue::from_str(header_value) {
                    if let Ok(header_name) = HeaderName::from_str(header_name) {
                      response_parts.headers.insert(header_name, header_value);
                    }
                  }
                }
              }
            }
          }
        }
        if let Ok(server_string) = HeaderValue::from_str(SERVER_SOFTWARE) {
          response_parts.headers.insert(header::SERVER, server_string);
        };
        Ok(Response::from_parts(response_parts, response_body))
      } else {
        let response = Response::builder()
          .status(StatusCode::BAD_REQUEST)
          .body(Empty::new().map_err(|e| match e {}).boxed())
          .unwrap_or_default();

        if log_enabled {
          log_combined(
            &logger,
            socket_data.remote_addr.ip(),
            None,
            log_method,
            log_request_path,
            log_protocol,
            response.status().as_u16(),
            match response.headers().get(header::CONTENT_LENGTH) {
              Some(header_value) => match header_value.to_str() {
                Ok(header_value) => match header_value.parse::<u64>() {
                  Ok(content_length) => Some(content_length),
                  Err(_) => response.body().size_hint().exact(),
                },
                Err(_) => response.body().size_hint().exact(),
              },
              None => response.body().size_hint().exact(),
            },
            log_referrer,
            log_user_agent,
          )
          .await;
        }
        let (mut response_parts, response_body) = response.into_parts();
        if let Some(custom_headers_hash) = combined_config.get("customHeaders").as_hash() {
          let custom_headers_hash_iter = custom_headers_hash.iter();
          for (header_name, header_value) in custom_headers_hash_iter {
            if let Some(header_name) = header_name.as_str() {
              if let Some(header_value) = header_value.as_str() {
                if !response_parts.headers.contains_key(header_name) {
                  if let Ok(header_value) = HeaderValue::from_str(header_value) {
                    if let Ok(header_name) = HeaderName::from_str(header_name) {
                      response_parts.headers.insert(header_name, header_value);
                    }
                  }
                }
              }
            }
          }
        }
        if let Ok(server_string) = HeaderValue::from_str(SERVER_SOFTWARE) {
          response_parts.headers.insert(header::SERVER, server_string);
        };
        Ok(Response::from_parts(response_parts, response_body))
      }
    } else {
      let response = Response::builder()
        .status(StatusCode::NOT_IMPLEMENTED)
        .body(Empty::new().map_err(|e| match e {}).boxed())
        .unwrap_or_default();

      if log_enabled {
        log_combined(
          &logger,
          socket_data.remote_addr.ip(),
          None,
          log_method,
          log_request_path,
          log_protocol,
          response.status().as_u16(),
          match response.headers().get(header::CONTENT_LENGTH) {
            Some(header_value) => match header_value.to_str() {
              Ok(header_value) => match header_value.parse::<u64>() {
                Ok(content_length) => Some(content_length),
                Err(_) => response.body().size_hint().exact(),
              },
              Err(_) => response.body().size_hint().exact(),
            },
            None => response.body().size_hint().exact(),
          },
          log_referrer,
          log_user_agent,
        )
        .await;
      }
      let (mut response_parts, response_body) = response.into_parts();
      if let Some(custom_headers_hash) = combined_config.get("customHeaders").as_hash() {
        let custom_headers_hash_iter = custom_headers_hash.iter();
        for (header_name, header_value) in custom_headers_hash_iter {
          if let Some(header_name) = header_name.as_str() {
            if let Some(header_value) = header_value.as_str() {
              if !response_parts.headers.contains_key(header_name) {
                if let Ok(header_value) = HeaderValue::from_str(header_value) {
                  if let Ok(header_name) = HeaderName::from_str(header_name) {
                    response_parts.headers.insert(header_name, header_value);
                  }
                }
              }
            }
          }
        }
      }
      if let Ok(server_string) = HeaderValue::from_str(SERVER_SOFTWARE) {
        response_parts.headers.insert(header::SERVER, server_string);
      };
      Ok(Response::from_parts(response_parts, response_body))
    }
  } else {
    let is_websocket_request = is_upgrade_request(&request);
    let mut request_data = RequestData::new(request, None);
    let mut latest_auth_data = None;
    let mut executed_handlers = Vec::new();
    for mut handlers in handlers_vec {
      if is_websocket_request && handlers.does_websocket_requests(&combined_config, &socket_data) {
        let (request, _) = request_data.into_parts();

        // Variables moved to before "tokio::spawn" to avoid issues with moved values
        let client_ip = socket_data.remote_addr.ip();
        let custom_headers_yaml = combined_config.get("customHeaders");
        let request_uri = request.uri().to_owned();

        let (original_response, websocket) = match hyper_tungstenite::upgrade(request, None) {
          Ok(data) => data,
          Err(err) => {
            error_logger
              .log(&format!("Error while upgrading WebSocket request: {}", err))
              .await;
            let response = Response::builder()
              .status(StatusCode::INTERNAL_SERVER_ERROR)
              .body(
                Full::new(Bytes::from(generate_default_error_page(
                  StatusCode::INTERNAL_SERVER_ERROR,
                  None,
                )))
                .map_err(|e| match e {})
                .boxed(),
              )
              .unwrap_or_default();

            if log_enabled {
              log_combined(
                &logger,
                socket_data.remote_addr.ip(),
                None,
                log_method,
                log_request_path,
                log_protocol,
                response.status().as_u16(),
                match response.headers().get(header::CONTENT_LENGTH) {
                  Some(header_value) => match header_value.to_str() {
                    Ok(header_value) => match header_value.parse::<u64>() {
                      Ok(content_length) => Some(content_length),
                      Err(_) => response.body().size_hint().exact(),
                    },
                    Err(_) => response.body().size_hint().exact(),
                  },
                  None => response.body().size_hint().exact(),
                },
                log_referrer,
                log_user_agent,
              )
              .await;
            }
            let (mut response_parts, response_body) = response.into_parts();
            if let Some(custom_headers_hash) = combined_config.get("customHeaders").as_hash() {
              let custom_headers_hash_iter = custom_headers_hash.iter();
              for (header_name, header_value) in custom_headers_hash_iter {
                if let Some(header_name) = header_name.as_str() {
                  if let Some(header_value) = header_value.as_str() {
                    if !response_parts.headers.contains_key(header_name) {
                      if let Ok(header_value) = HeaderValue::from_str(header_value) {
                        if let Ok(header_name) = HeaderName::from_str(header_name) {
                          response_parts.headers.insert(header_name, header_value);
                        }
                      }
                    }
                  }
                }
              }
            }
            if let Ok(server_string) = HeaderValue::from_str(SERVER_SOFTWARE) {
              response_parts.headers.insert(header::SERVER, server_string);
            };
            return Ok(Response::from_parts(response_parts, response_body));
          }
        };

        tokio::spawn(async move {
          let result = handlers
            .websocket_request_handler(
              websocket,
              &request_uri,
              &combined_config,
              &socket_data,
              &error_logger,
            )
            .await;
          match result {
            Ok(_) => (),
            Err(err) => {
              error_logger
                .log(&format!("Unexpected error for WebSocket request: {}", err))
                .await;
            }
          }
        });

        let response = original_response.map(|body| body.map_err(|err| match err {}).boxed());

        if log_enabled {
          log_combined(
            &logger,
            client_ip,
            None,
            log_method,
            log_request_path,
            log_protocol,
            response.status().as_u16(),
            match response.headers().get(header::CONTENT_LENGTH) {
              Some(header_value) => match header_value.to_str() {
                Ok(header_value) => match header_value.parse::<u64>() {
                  Ok(content_length) => Some(content_length),
                  Err(_) => response.body().size_hint().exact(),
                },
                Err(_) => response.body().size_hint().exact(),
              },
              None => response.body().size_hint().exact(),
            },
            log_referrer,
            log_user_agent,
          )
          .await;
        }

        let (mut response_parts, response_body) = response.into_parts();
        if let Some(custom_headers_hash) = custom_headers_yaml.as_hash() {
          let custom_headers_hash_iter = custom_headers_hash.iter();
          for (header_name, header_value) in custom_headers_hash_iter {
            if let Some(header_name) = header_name.as_str() {
              if let Some(header_value) = header_value.as_str() {
                if !response_parts.headers.contains_key(header_name) {
                  if let Ok(header_value) = HeaderValue::from_str(header_value) {
                    if let Ok(header_name) = HeaderName::from_str(header_name) {
                      response_parts.headers.insert(header_name, header_value);
                    }
                  }
                }
              }
            }
          }
        }
        if let Ok(server_string) = HeaderValue::from_str(SERVER_SOFTWARE) {
          response_parts.headers.insert(header::SERVER, server_string);
        };

        return Ok(Response::from_parts(response_parts, response_body));
      }

      let response_result = match is_proxy_request {
        true => {
          handlers
            .proxy_request_handler(request_data, &combined_config, &socket_data, &error_logger)
            .await
        }
        false => {
          handlers
            .request_handler(request_data, &combined_config, &socket_data, &error_logger)
            .await
        }
      };

      executed_handlers.push(handlers);
      match response_result {
        Ok(response) => {
          let (
            request_option,
            auth_data,
            response,
            status,
            headers,
            new_remote_address,
            parallel_fn,
          ) = response.into_parts();
          latest_auth_data = auth_data.clone();
          if let Some(new_remote_address) = new_remote_address {
            socket_data.remote_addr = new_remote_address;
          };
          if let Some(parallel_fn) = parallel_fn {
            // Spawn the function in the web server's Tokio runtime.
            // We have implemented parallel_fn parameter in the ResponseData
            // because tokio::spawn doesn't work on dynamic libraries,
            // see https://github.com/tokio-rs/tokio/issues/6927
            tokio::spawn(parallel_fn);
          }
          match response {
            Some(response) => {
              let (mut response_parts, response_body) = response.into_parts();
              if let Some(custom_headers_hash) = combined_config.get("customHeaders").as_hash() {
                let custom_headers_hash_iter = custom_headers_hash.iter();
                for (header_name, header_value) in custom_headers_hash_iter {
                  if let Some(header_name) = header_name.as_str() {
                    if let Some(header_value) = header_value.as_str() {
                      if !response_parts.headers.contains_key(header_name) {
                        if let Ok(header_value) = HeaderValue::from_str(header_value) {
                          if let Ok(header_name) = HeaderName::from_str(header_name) {
                            response_parts.headers.insert(header_name, header_value);
                          }
                        }
                      }
                    }
                  }
                }
              }
              if let Ok(server_string) = HeaderValue::from_str(SERVER_SOFTWARE) {
                response_parts.headers.insert(header::SERVER, server_string);
              };
              let mut response = Response::from_parts(response_parts, response_body);

              while let Some(mut executed_handler) = executed_handlers.pop() {
                let response_status = match is_proxy_request {
                  true => {
                    executed_handler
                      .proxy_response_modifying_handler(response)
                      .await
                  }
                  false => executed_handler.response_modifying_handler(response).await,
                };
                response = match response_status {
                  Ok(response) => response,
                  Err(err) => {
                    if error_log_enabled {
                      logger
                        .send(LogMessage::new(
                          format!("Unexpected error while serving a request: {}", err),
                          true,
                        ))
                        .await
                        .unwrap_or_default();
                    }

                    let response = generate_error_response(
                      StatusCode::INTERNAL_SERVER_ERROR,
                      &combined_config,
                      &headers,
                    )
                    .await;
                    if log_enabled {
                      log_combined(
                        &logger,
                        socket_data.remote_addr.ip(),
                        auth_data,
                        log_method,
                        log_request_path,
                        log_protocol,
                        response.status().as_u16(),
                        match response.headers().get(header::CONTENT_LENGTH) {
                          Some(header_value) => match header_value.to_str() {
                            Ok(header_value) => match header_value.parse::<u64>() {
                              Ok(content_length) => Some(content_length),
                              Err(_) => response.body().size_hint().exact(),
                            },
                            Err(_) => response.body().size_hint().exact(),
                          },
                          None => response.body().size_hint().exact(),
                        },
                        log_referrer,
                        log_user_agent,
                      )
                      .await;
                    }
                    let (mut response_parts, response_body) = response.into_parts();
                    if let Some(custom_headers_hash) =
                      combined_config.get("customHeaders").as_hash()
                    {
                      let custom_headers_hash_iter = custom_headers_hash.iter();
                      for (header_name, header_value) in custom_headers_hash_iter {
                        if let Some(header_name) = header_name.as_str() {
                          if let Some(header_value) = header_value.as_str() {
                            if !response_parts.headers.contains_key(header_name) {
                              if let Ok(header_value) = HeaderValue::from_str(header_value) {
                                if let Ok(header_name) = HeaderName::from_str(header_name) {
                                  response_parts.headers.insert(header_name, header_value);
                                }
                              }
                            }
                          }
                        }
                      }
                    }
                    if let Ok(server_string) = HeaderValue::from_str(SERVER_SOFTWARE) {
                      response_parts.headers.insert(header::SERVER, server_string);
                    };
                    return Ok(Response::from_parts(response_parts, response_body));
                  }
                };
              }

              if log_enabled {
                log_combined(
                  &logger,
                  socket_data.remote_addr.ip(),
                  auth_data,
                  log_method,
                  log_request_path,
                  log_protocol,
                  response.status().as_u16(),
                  match response.headers().get(header::CONTENT_LENGTH) {
                    Some(header_value) => match header_value.to_str() {
                      Ok(header_value) => match header_value.parse::<u64>() {
                        Ok(content_length) => Some(content_length),
                        Err(_) => response.body().size_hint().exact(),
                      },
                      Err(_) => response.body().size_hint().exact(),
                    },
                    None => response.body().size_hint().exact(),
                  },
                  log_referrer,
                  log_user_agent,
                )
                .await;
              }

              return Ok(response);
            }
            None => match status {
              Some(status) => {
                let response = generate_error_response(status, &combined_config, &headers).await;
                let (mut response_parts, response_body) = response.into_parts();
                if let Some(custom_headers_hash) = combined_config.get("customHeaders").as_hash() {
                  let custom_headers_hash_iter = custom_headers_hash.iter();
                  for (header_name, header_value) in custom_headers_hash_iter {
                    if let Some(header_name) = header_name.as_str() {
                      if let Some(header_value) = header_value.as_str() {
                        if !response_parts.headers.contains_key(header_name) {
                          if let Ok(header_value) = HeaderValue::from_str(header_value) {
                            if let Ok(header_name) = HeaderName::from_str(header_name) {
                              response_parts.headers.insert(header_name, header_value);
                            }
                          }
                        }
                      }
                    }
                  }
                }
                if let Ok(server_string) = HeaderValue::from_str(SERVER_SOFTWARE) {
                  response_parts.headers.insert(header::SERVER, server_string);
                };
                let mut response = Response::from_parts(response_parts, response_body);

                while let Some(mut executed_handler) = executed_handlers.pop() {
                  let response_status = match is_proxy_request {
                    true => {
                      executed_handler
                        .proxy_response_modifying_handler(response)
                        .await
                    }
                    false => executed_handler.response_modifying_handler(response).await,
                  };
                  response = match response_status {
                    Ok(response) => response,
                    Err(err) => {
                      if error_log_enabled {
                        logger
                          .send(LogMessage::new(
                            format!("Unexpected error while serving a request: {}", err),
                            true,
                          ))
                          .await
                          .unwrap_or_default();
                      }

                      let response = generate_error_response(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        &combined_config,
                        &headers,
                      )
                      .await;
                      if log_enabled {
                        log_combined(
                          &logger,
                          socket_data.remote_addr.ip(),
                          auth_data,
                          log_method,
                          log_request_path,
                          log_protocol,
                          response.status().as_u16(),
                          match response.headers().get(header::CONTENT_LENGTH) {
                            Some(header_value) => match header_value.to_str() {
                              Ok(header_value) => match header_value.parse::<u64>() {
                                Ok(content_length) => Some(content_length),
                                Err(_) => response.body().size_hint().exact(),
                              },
                              Err(_) => response.body().size_hint().exact(),
                            },
                            None => response.body().size_hint().exact(),
                          },
                          log_referrer,
                          log_user_agent,
                        )
                        .await;
                      }
                      let (mut response_parts, response_body) = response.into_parts();
                      if let Some(custom_headers_hash) =
                        combined_config.get("customHeaders").as_hash()
                      {
                        let custom_headers_hash_iter = custom_headers_hash.iter();
                        for (header_name, header_value) in custom_headers_hash_iter {
                          if let Some(header_name) = header_name.as_str() {
                            if let Some(header_value) = header_value.as_str() {
                              if !response_parts.headers.contains_key(header_name) {
                                if let Ok(header_value) = HeaderValue::from_str(header_value) {
                                  if let Ok(header_name) = HeaderName::from_str(header_name) {
                                    response_parts.headers.insert(header_name, header_value);
                                  }
                                }
                              }
                            }
                          }
                        }
                      }
                      if let Ok(server_string) = HeaderValue::from_str(SERVER_SOFTWARE) {
                        response_parts.headers.insert(header::SERVER, server_string);
                      };
                      return Ok(Response::from_parts(response_parts, response_body));
                    }
                  };
                }

                if log_enabled {
                  log_combined(
                    &logger,
                    socket_data.remote_addr.ip(),
                    auth_data,
                    log_method,
                    log_request_path,
                    log_protocol,
                    response.status().as_u16(),
                    match response.headers().get(header::CONTENT_LENGTH) {
                      Some(header_value) => match header_value.to_str() {
                        Ok(header_value) => match header_value.parse::<u64>() {
                          Ok(content_length) => Some(content_length),
                          Err(_) => response.body().size_hint().exact(),
                        },
                        Err(_) => response.body().size_hint().exact(),
                      },
                      None => response.body().size_hint().exact(),
                    },
                    log_referrer,
                    log_user_agent,
                  )
                  .await;
                }
                return Ok(response);
              }
              None => match request_option {
                Some(request) => {
                  request_data = RequestData::new(request, auth_data);
                  continue;
                }
                None => {
                  break;
                }
              },
            },
          }
        }
        Err(err) => {
          let response =
            generate_error_response(StatusCode::INTERNAL_SERVER_ERROR, &combined_config, &None)
              .await;

          let (mut response_parts, response_body) = response.into_parts();
          if let Some(custom_headers_hash) = combined_config.get("customHeaders").as_hash() {
            let custom_headers_hash_iter = custom_headers_hash.iter();
            for (header_name, header_value) in custom_headers_hash_iter {
              if let Some(header_name) = header_name.as_str() {
                if let Some(header_value) = header_value.as_str() {
                  if !response_parts.headers.contains_key(header_name) {
                    if let Ok(header_value) = HeaderValue::from_str(header_value) {
                      if let Ok(header_name) = HeaderName::from_str(header_name) {
                        response_parts.headers.insert(header_name, header_value);
                      }
                    }
                  }
                }
              }
            }
          }
          if let Ok(server_string) = HeaderValue::from_str(SERVER_SOFTWARE) {
            response_parts.headers.insert(header::SERVER, server_string);
          };

          let mut response = Response::from_parts(response_parts, response_body);

          while let Some(mut executed_handler) = executed_handlers.pop() {
            let response_status = match is_proxy_request {
              true => {
                executed_handler
                  .proxy_response_modifying_handler(response)
                  .await
              }
              false => executed_handler.response_modifying_handler(response).await,
            };
            response = match response_status {
              Ok(response) => response,
              Err(err) => {
                if error_log_enabled {
                  logger
                    .send(LogMessage::new(
                      format!("Unexpected error while serving a request: {}", err),
                      true,
                    ))
                    .await
                    .unwrap_or_default();
                }

                let response = generate_error_response(
                  StatusCode::INTERNAL_SERVER_ERROR,
                  &combined_config,
                  &None,
                )
                .await;
                if log_enabled {
                  log_combined(
                    &logger,
                    socket_data.remote_addr.ip(),
                    latest_auth_data,
                    log_method,
                    log_request_path,
                    log_protocol,
                    response.status().as_u16(),
                    match response.headers().get(header::CONTENT_LENGTH) {
                      Some(header_value) => match header_value.to_str() {
                        Ok(header_value) => match header_value.parse::<u64>() {
                          Ok(content_length) => Some(content_length),
                          Err(_) => response.body().size_hint().exact(),
                        },
                        Err(_) => response.body().size_hint().exact(),
                      },
                      None => response.body().size_hint().exact(),
                    },
                    log_referrer,
                    log_user_agent,
                  )
                  .await;
                }
                let (mut response_parts, response_body) = response.into_parts();
                if let Some(custom_headers_hash) = combined_config.get("customHeaders").as_hash() {
                  let custom_headers_hash_iter = custom_headers_hash.iter();
                  for (header_name, header_value) in custom_headers_hash_iter {
                    if let Some(header_name) = header_name.as_str() {
                      if let Some(header_value) = header_value.as_str() {
                        if !response_parts.headers.contains_key(header_name) {
                          if let Ok(header_value) = HeaderValue::from_str(header_value) {
                            if let Ok(header_name) = HeaderName::from_str(header_name) {
                              response_parts.headers.insert(header_name, header_value);
                            }
                          }
                        }
                      }
                    }
                  }
                }
                if let Ok(server_string) = HeaderValue::from_str(SERVER_SOFTWARE) {
                  response_parts.headers.insert(header::SERVER, server_string);
                };
                return Ok(Response::from_parts(response_parts, response_body));
              }
            };
          }

          if error_log_enabled {
            logger
              .send(LogMessage::new(
                format!("Unexpected error while serving a request: {}", err),
                true,
              ))
              .await
              .unwrap_or_default();
          }

          if log_enabled {
            log_combined(
              &logger,
              socket_data.remote_addr.ip(),
              latest_auth_data,
              log_method,
              log_request_path,
              log_protocol,
              response.status().as_u16(),
              match response.headers().get(header::CONTENT_LENGTH) {
                Some(header_value) => match header_value.to_str() {
                  Ok(header_value) => match header_value.parse::<u64>() {
                    Ok(content_length) => Some(content_length),
                    Err(_) => response.body().size_hint().exact(),
                  },
                  Err(_) => response.body().size_hint().exact(),
                },
                None => response.body().size_hint().exact(),
              },
              log_referrer,
              log_user_agent,
            )
            .await;
          }
          return Ok(response);
        }
      }
    }

    let response = generate_error_response(StatusCode::NOT_FOUND, &combined_config, &None).await;

    let (mut response_parts, response_body) = response.into_parts();
    if let Some(custom_headers_hash) = combined_config.get("customHeaders").as_hash() {
      let custom_headers_hash_iter = custom_headers_hash.iter();
      for (header_name, header_value) in custom_headers_hash_iter {
        if let Some(header_name) = header_name.as_str() {
          if let Some(header_value) = header_value.as_str() {
            if !response_parts.headers.contains_key(header_name) {
              if let Ok(header_value) = HeaderValue::from_str(header_value) {
                if let Ok(header_name) = HeaderName::from_str(header_name) {
                  response_parts.headers.insert(header_name, header_value);
                }
              }
            }
          }
        }
      }
    }
    if let Ok(server_string) = HeaderValue::from_str(SERVER_SOFTWARE) {
      response_parts.headers.insert(header::SERVER, server_string);
    };
    let mut response = Response::from_parts(response_parts, response_body);

    while let Some(mut executed_handler) = executed_handlers.pop() {
      let response_status = match is_proxy_request {
        true => {
          executed_handler
            .proxy_response_modifying_handler(response)
            .await
        }
        false => executed_handler.response_modifying_handler(response).await,
      };
      response = match response_status {
        Ok(response) => response,
        Err(err) => {
          if error_log_enabled {
            logger
              .send(LogMessage::new(
                format!("Unexpected error while serving a request: {}", err),
                true,
              ))
              .await
              .unwrap_or_default();
          }

          let response =
            generate_error_response(StatusCode::INTERNAL_SERVER_ERROR, &combined_config, &None)
              .await;
          if log_enabled {
            log_combined(
              &logger,
              socket_data.remote_addr.ip(),
              latest_auth_data,
              log_method,
              log_request_path,
              log_protocol,
              response.status().as_u16(),
              match response.headers().get(header::CONTENT_LENGTH) {
                Some(header_value) => match header_value.to_str() {
                  Ok(header_value) => match header_value.parse::<u64>() {
                    Ok(content_length) => Some(content_length),
                    Err(_) => response.body().size_hint().exact(),
                  },
                  Err(_) => response.body().size_hint().exact(),
                },
                None => response.body().size_hint().exact(),
              },
              log_referrer,
              log_user_agent,
            )
            .await;
          }
          let (mut response_parts, response_body) = response.into_parts();
          if let Some(custom_headers_hash) = combined_config.get("customHeaders").as_hash() {
            let custom_headers_hash_iter = custom_headers_hash.iter();
            for (header_name, header_value) in custom_headers_hash_iter {
              if let Some(header_name) = header_name.as_str() {
                if let Some(header_value) = header_value.as_str() {
                  if !response_parts.headers.contains_key(header_name) {
                    if let Ok(header_value) = HeaderValue::from_str(header_value) {
                      if let Ok(header_name) = HeaderName::from_str(header_name) {
                        response_parts.headers.insert(header_name, header_value);
                      }
                    }
                  }
                }
              }
            }
          }
          if let Ok(server_string) = HeaderValue::from_str(SERVER_SOFTWARE) {
            response_parts.headers.insert(header::SERVER, server_string);
          };
          return Ok(Response::from_parts(response_parts, response_body));
        }
      };
    }

    if log_enabled {
      log_combined(
        &logger,
        socket_data.remote_addr.ip(),
        latest_auth_data,
        log_method,
        log_request_path,
        log_protocol,
        response.status().as_u16(),
        match response.headers().get(header::CONTENT_LENGTH) {
          Some(header_value) => match header_value.to_str() {
            Ok(header_value) => match header_value.parse::<u64>() {
              Ok(content_length) => Some(content_length),
              Err(_) => response.body().size_hint().exact(),
            },
            Err(_) => response.body().size_hint().exact(),
          },
          None => response.body().size_hint().exact(),
        },
        log_referrer,
        log_user_agent,
      )
      .await;
    }
    Ok(response)
  }
}

#[allow(clippy::too_many_arguments)]
pub async fn request_handler(
  request: Request<BoxBody<Bytes, hyper::Error>>,
  remote_address: SocketAddr,
  local_address: SocketAddr,
  encrypted: bool,
  global_config_root: Arc<ServerConfigRoot>,
  host_config: Arc<Yaml>,
  logger: Sender<LogMessage>,
  handlers_vec: Vec<Box<dyn ServerModuleHandlers + Send>>,
) -> Result<Response<BoxBody<Bytes, std::io::Error>>, anyhow::Error> {
  let timeout_yaml = global_config_root.get("timeout");
  if timeout_yaml.is_null() {
    request_handler_wrapped(
      request,
      remote_address,
      local_address,
      encrypted,
      global_config_root,
      host_config,
      logger,
      handlers_vec,
    )
    .await
    .map_err(|e| anyhow::anyhow!(e))
  } else {
    let timeout_millis = timeout_yaml.as_i64().unwrap_or(300000) as u64;
    match timeout(
      Duration::from_millis(timeout_millis),
      request_handler_wrapped(
        request,
        remote_address,
        local_address,
        encrypted,
        global_config_root,
        host_config,
        logger,
        handlers_vec,
      ),
    )
    .await
    {
      Ok(response) => response.map_err(|e| anyhow::anyhow!(e)),
      Err(_) => Err(anyhow::anyhow!("The client or server has timed out")),
    }
  }
}
