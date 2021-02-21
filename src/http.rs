#![warn(clippy::all)]

use actix_web::{web, guard, http::{header, StatusCode}, middleware::{Compress, Logger, DefaultHeaders, NormalizePath, TrailingSlash}, App, Scope, HttpRequest, HttpResponse, HttpServer};
use futures::future::Either;
use log::{trace, debug, info};
use rustls::{NoClientAuth, ServerConfig, ResolvesServerCertUsingSNI, sign, sign::CertifiedKey, PrivateKey, Certificate};
use serde_derive::Deserialize;
use std::{iter, fs::File, collections::BTreeMap, net::SocketAddr, path::PathBuf, io::BufReader, sync::Arc, error::Error, boxed::Box, future, future::Future};

#[derive(Deserialize,Clone,Debug)]
#[serde(deny_unknown_fields)]
pub struct Vhost {
	pub host: String,

	#[serde(default)]
	pub files: Vec<Files>,

	#[serde(default)]
	pub redir: Vec<Redir>,

	pub tls: Option<Tls>,
}

#[derive(Deserialize,Clone,Debug)]
#[serde(deny_unknown_fields)]
pub struct Files {
	#[serde(default)]
	pub mount: String,

	pub file_dir: PathBuf,
}

#[derive(Deserialize,Clone,Debug)]
#[serde(deny_unknown_fields)]
pub struct Redir {
	#[serde(default)]
	pub target: String,

	pub dest: String,

	#[serde(default)]
	pub permanent: bool,
}

#[derive(Deserialize,Clone,Debug)]
#[serde(deny_unknown_fields)]
pub struct Tls {
	pub pemfiles: Vec<PathBuf>,
	pub http_dest: Option<String>,
}


#[derive(Deserialize,Clone,Debug)]
#[serde(deny_unknown_fields)]
pub struct Server {
	#[serde(default)]
	pub http_bind: Vec<SocketAddr>,

	#[serde(default)]
	pub tls_bind: Vec<SocketAddr>,

	#[serde(default = "default_server_log_format")]
	pub log_format: String,
}

fn default_server_log_format() -> String {
	"%{Host}i %a \"%r\" %s %b \"%{Referer}i\" \"%{User-Agent}i\" %D".to_string()
}

fn handle_redirect(req: HttpRequest, status: web::Data<StatusCode>, dest: web::Data<String>) -> HttpResponse {
	let mut dest = dest.to_string();
	for (_, segment) in req.match_info().iter() {
		dest = [&dest,"/",segment].concat()
	}

	HttpResponse::build(*status.as_ref())
		.append_header((header::LOCATION, dest))
		.finish()
}

fn handle_https_redirect(req: HttpRequest, dest: web::Data<String>) -> HttpResponse {
	HttpResponse::PermanentRedirect()
		.append_header((header::LOCATION, [dest.as_str(), req.path()].concat()))
		.finish()
}

fn create_certified_key(pemfiles: &[PathBuf]) -> Result<CertifiedKey, Box<dyn Error>> {
	let mut certs = Vec::new();
	let mut keys = Vec::new();
	for pemfile in pemfiles {
		let mut reader = BufReader::new(File::open(pemfile)?);
		for item in iter::from_fn(|| rustls_pemfile::read_one(&mut reader).transpose()) {
			match item? {
				rustls_pemfile::Item::X509Certificate(cert) => certs.push(Certificate(cert)),
				rustls_pemfile::Item::PKCS8Key(key) => keys.push(PrivateKey(key)),
				rustls_pemfile::Item::RSAKey(key) => keys.push(PrivateKey(key)),
			}
		}
	}

	let key = keys.get(0).ok_or("no valid keys found")?;
	let signingkey = sign::any_supported_type(key).or(Err("unable to parse key"))?;

	Ok(CertifiedKey::new(certs, Arc::new(signingkey)))
}

fn configure_vhost_scope(vhost: &Vhost, is_tls: bool) -> Option<Scope> {
	if is_tls && vhost.tls.is_none() {
		return None
	}

	let mut scope = web::scope("/")
		.guard(guard::Host(String::from(&vhost.host)));

	// https://github.com/rust-lang/rust/issues/53667
	if let Some(Tls{ http_dest: Some(dest), ..}) = &vhost.tls {
		if !is_tls {
			return Some(scope.data(dest.to_owned()).default_service(web::to(handle_https_redirect)))
		}
	}

	for redir in vhost.redir.to_owned() {
		let status = match redir.permanent {
			true => StatusCode::PERMANENT_REDIRECT,
			false => StatusCode::TEMPORARY_REDIRECT,
		};
		let target = match redir.target.as_ref() {
			"/" => "",
			_ => &redir.target,
		};
		scope = scope.service(
			web::resource(target)
				.data(status).data(redir.dest)
				.to(handle_redirect)
		)
	}

	// Potentially useful future features:
	// - https://github.com/actix/actix-web/issues/1718
	// - https://github.com/actix/actix-web/issues/2000
	for files in vhost.files.to_owned() {
		let mount = match files.mount.as_ref() {
			"/" => "",
			_ => &files.mount,
		};
		scope = scope.service(
			actix_files::Files::new(mount, &files.file_dir)
				.index_file("index.html")
				.prefer_utf8(true)
				.disable_content_disposition()
		)
	}
	
	Some(scope)
}

pub fn run_http_server(is_tls: bool, server: &Server, headers: &BTreeMap<String, String>, vhosts: &[Vhost]) -> Result<impl Future<Output = Result<(), std::io::Error>>, Box<dyn Error>> {
	let log_format = server.log_format.to_owned();
	let vhosts_copy = vhosts.to_owned();
	let headers_copy = headers.to_owned();

	let mut http_server = HttpServer::new(move || {
		match is_tls {
			true => trace!("generating https application builder"),
			false => trace!("generating http application builder"),
		}

		let mut default_headers = DefaultHeaders::new();
		for (key, val) in &headers_copy {
			default_headers = default_headers.header(key, val);
		}

		let mut app = App::new()
			.wrap(Logger::new(&log_format))
			.wrap(default_headers)
			.wrap(NormalizePath::new(TrailingSlash::MergeOnly))
			.wrap(Compress::default());

		for vhost in &vhosts_copy {
			app = match configure_vhost_scope(&vhost, is_tls) {
				Some(scope) => app.service(scope),
				None => app,
			};
		}

		app
	});

	match is_tls {
		true => {
			if server.tls_bind.is_empty() {
				trace!("tls_bind is empty, skipping https init");
				return Ok(Either::Left(future::ready(Ok(()))));
			}
			info!("Starting HTTPS Server");

			debug!("loading tls certificates");
			let mut resolver = ResolvesServerCertUsingSNI::new();
			for vhost in vhosts {
				if let Some(tls) = &vhost.tls {
					let keypair = create_certified_key(&tls.pemfiles)?;
					resolver.add(&vhost.host, keypair)?;
				}
			}

			let mut tlsconf = ServerConfig::new(NoClientAuth::new());
			tlsconf.cert_resolver = Arc::new(resolver);

			for addr in &server.tls_bind {
				http_server = http_server.bind_rustls(addr, tlsconf.to_owned())?
			}
		},
		false => {
			if server.http_bind.is_empty() {
				trace!("http_bind is empty, skipping http init");
				return Ok(Either::Left(future::ready(Ok(()))));
			}
			info!("Starting HTTP Server");

			for addr in &server.http_bind {
				http_server = http_server.bind(addr)?
			}
		},
	}

	Ok(Either::Right(http_server.run()))
}

