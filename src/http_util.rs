// Copyright 2018-2019 the Deno authors. All rights reserved. MIT license.
use crate::errors;
use crate::errors::{DenoError, DenoResult};
use crate::tokio_util;

use futures::future::{loop_fn, Loop};
use futures::{future, Future, Stream};
use hyper;
use hyper::client::{Client, HttpConnector};
use hyper::header::CONTENT_TYPE;
use hyper::Request;
use hyper::Uri;
use hyper_proxy::{Intercept, Proxy, ProxyConnector};
use hyper_rustls;

type InnerConnector = hyper_rustls::HttpsConnector<HttpConnector>;
type Connector = ProxyConnector<InnerConnector>;

lazy_static! {
  static ref CONNECTOR: Connector = {
    let num_dns_threads = 4;
    let inner = InnerConnector::new(num_dns_threads);
    let mut outer = Connector::unsecured(inner);

    // if let Ok(list) = env_var("no_proxy") {
    //   if list == "*" {
    //     let proxy_uri = ("http://example.com").parse().unwrap();
    //     outer.add_proxy(Proxy::new(Intercept::None, proxy_uri));
    //   } else {
    //
    //   }
    // }

    // Only lowercase according to curl
    if let Ok(target) = std::env::var("http_proxy") {
      let proxy_uri = target.parse().unwrap();
      outer.add_proxy(Proxy::new(Intercept::Http, proxy_uri));
    }

    if let Ok(target) = env_var("https_proxy") {
      let proxy_uri = target.parse().unwrap();
      outer.add_proxy(Proxy::new(Intercept::Https, proxy_uri));
    }

    if let Ok(target) = env_var("all_proxy") {
      let proxy_uri = target.parse().unwrap();
      outer.add_proxy(Proxy::new(Intercept::All, proxy_uri));
    }

    return outer;
  };
}

fn env_var(key: &str) -> Result<String, std::env::VarError> {
  match std::env::var(key.to_lowercase()) {
    Ok(target) => Ok(target),
    Err(err) => match std::env::var(key.to_uppercase()) {
      Ok(target) => Ok(target),
      Err(_) => Err(err),
    },
  }
}

// Add headers for HTTP requests when proxied
pub fn add_proxy_headers<T>(req: &mut Request<T>) {
  if let Some(headers) = CONNECTOR.http_headers(req.uri()) {
    req.headers_mut().extend(headers.clone().into_iter());
  }
}

pub fn get_client() -> Client<Connector, hyper::Body> {
  // TODO use Hyper's connection pool.
  let c = CONNECTOR.clone();
  Client::builder().build(c)
}

/// Construct the next uri based on base uri and location header fragment
/// See <https://tools.ietf.org/html/rfc3986#section-4.2>
fn resolve_uri_from_location(base_uri: &Uri, location: &str) -> Uri {
  if location.starts_with("http://") || location.starts_with("https://") {
    // absolute uri
    location
      .parse::<Uri>()
      .expect("provided redirect url should be a valid url")
  } else if location.starts_with("//") {
    // "//" authority path-abempty
    format!("{}:{}", base_uri.scheme_part().unwrap().as_str(), location)
      .parse::<Uri>()
      .expect("provided redirect url should be a valid url")
  } else if location.starts_with('/') {
    // path-absolute
    let mut new_uri_parts = base_uri.clone().into_parts();
    new_uri_parts.path_and_query = Some(location.parse().unwrap());
    Uri::from_parts(new_uri_parts).unwrap()
  } else {
    // assuming path-noscheme | path-empty
    let mut new_uri_parts = base_uri.clone().into_parts();
    new_uri_parts.path_and_query =
      Some(format!("{}/{}", base_uri.path(), location).parse().unwrap());
    Uri::from_parts(new_uri_parts).unwrap()
  }
}

// The CodeFetch message is used to load HTTP javascript resources and expects a
// synchronous response, this utility method supports that.
pub fn fetch_sync_string(module_name: &str) -> DenoResult<(String, String)> {
  let url = module_name.parse::<Uri>().unwrap();
  let client = get_client();
  // TODO(kevinkassimo): consider set a max redirection counter
  // to avoid bouncing between 2 or more urls
  let fetch_future = loop_fn((client, url), |(client, url)| {
    let body = hyper::body::Body::default();
    let mut req = Request::new(body);
    *req.uri_mut() = url.clone();
    add_proxy_headers(&mut req);

    client
      .request(req)
      .map_err(DenoError::from)
      .and_then(move |response| {
        if response.status().is_redirection() {
          let location_string = response
            .headers()
            .get("location")
            .expect("url redirection should provide 'location' header")
            .to_str()
            .unwrap()
            .to_string();
          debug!("Redirecting to {}...", &location_string);
          let new_url = resolve_uri_from_location(&url, &location_string);
          return Ok(Loop::Continue((client, new_url)));
        }
        if !response.status().is_success() {
          return Err(errors::new(
            errors::ErrorKind::NotFound,
            "module not found".to_string(),
          ));
        }
        Ok(Loop::Break(response))
      })
  })
  .and_then(|response| {
    let content_type = response
      .headers()
      .get(CONTENT_TYPE)
      .map(|content_type| content_type.to_str().unwrap().to_string());
    let body = response
      .into_body()
      .concat2()
      .map(|body| String::from_utf8(body.to_vec()).unwrap())
      .map_err(DenoError::from);
    body.join(future::ok(content_type))
  })
  .and_then(|(body_string, maybe_content_type)| {
    future::ok((body_string, maybe_content_type.unwrap()))
  });

  tokio_util::block_on(fetch_future)
}

#[test]
fn test_fetch_sync_string() {
  // Relies on external http server. See tools/http_server.py
  tokio_util::init(|| {
    let (p, m) =
      fetch_sync_string("http://127.0.0.1:4545/package.json").unwrap();
    println!("package.json len {}", p.len());
    assert!(p.len() > 1);
    assert!(m == "application/json")
  });
}

#[test]
fn test_fetch_sync_string_with_redirect() {
  // Relies on external http server. See tools/http_server.py
  tokio_util::init(|| {
    let (p, m) =
      fetch_sync_string("http://127.0.0.1:4546/package.json").unwrap();
    println!("package.json len {}", p.len());
    assert!(p.len() > 1);
    assert!(m == "application/json")
  });
}

#[test]
fn test_resolve_uri_from_location_full_1() {
  let url = "http://deno.land".parse::<Uri>().unwrap();
  let new_uri = resolve_uri_from_location(&url, "http://golang.org");
  assert_eq!(new_uri.host().unwrap(), "golang.org");
}

#[test]
fn test_resolve_uri_from_location_full_2() {
  let url = "https://deno.land".parse::<Uri>().unwrap();
  let new_uri = resolve_uri_from_location(&url, "https://golang.org");
  assert_eq!(new_uri.host().unwrap(), "golang.org");
}

#[test]
fn test_resolve_uri_from_location_relative_1() {
  let url = "http://deno.land/x".parse::<Uri>().unwrap();
  let new_uri = resolve_uri_from_location(&url, "//rust-lang.org/en-US");
  assert_eq!(new_uri.host().unwrap(), "rust-lang.org");
  assert_eq!(new_uri.path(), "/en-US");
}

#[test]
fn test_resolve_uri_from_location_relative_2() {
  let url = "http://deno.land/x".parse::<Uri>().unwrap();
  let new_uri = resolve_uri_from_location(&url, "/y");
  assert_eq!(new_uri.host().unwrap(), "deno.land");
  assert_eq!(new_uri.path(), "/y");
}

#[test]
fn test_resolve_uri_from_location_relative_3() {
  let url = "http://deno.land/x".parse::<Uri>().unwrap();
  let new_uri = resolve_uri_from_location(&url, "z");
  assert_eq!(new_uri.host().unwrap(), "deno.land");
  assert_eq!(new_uri.path(), "/x/z");
}
