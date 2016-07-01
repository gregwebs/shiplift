//! Shiplift is a multi-transport utility for maneuvering [docker](https://www.docker.com/) containers
//!
//! # examples
//!
//! ```no_run
//! extern crate shiplift;
//!
//! let docker = shiplift::Docker::new();
//! let images = docker.images().list(&Default::default()).unwrap();
//! println!("docker images in stock");
//! for i in images {
//!   println!("{:?}", i.RepoTags);
//! }
//! ```

#[macro_use]
extern crate log;
#[macro_use]
extern crate mime;
#[macro_use]
extern crate hyper;
extern crate flate2;
extern crate hyperlocal;
extern crate jed;
extern crate openssl;
extern crate rustc_serialize;
extern crate url;
extern crate tar;

pub mod builder;
pub mod rep;
pub mod transport;
pub mod errors;

mod tarball;

pub use errors::Error;
pub use builder::{BuildOptions, ContainerOptions, ContainerListOptions, ContainerFilter,
                  EventsOptions, ImageFilter, ImageListOptions, LogsOptions,
                  PullOptions, RmContainerOptions
                  };
use hyper::{Client, Url};
use hyper::header::ContentType;
use hyper::net::{HttpsConnector, Openssl};
use hyper::method::Method;
use hyperlocal::UnixSocketConnector;
use openssl::x509::X509FileType;
use openssl::ssl::{SslContext, SslMethod};
use rep::Image as ImageRep;
use rep::{PullOutput, PullInfo, BuildOutput, Change, ContainerCreateInfo, ContainerDetails,
          Container as ContainerRep, Event, Exit, History, ImageDetails, Info, SearchResult,
          Stats, Status, Top, Version};
use rustc_serialize::json::{self, Json};
use std::env::{self, VarError};
use std::io::Read;
use std::iter::IntoIterator;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use transport::{tar, Transport};
use hyper::client::Body;
use url::{form_urlencoded, Host, RelativeSchemeData, SchemeData};

/// Represents the result of all docker operations
pub type Result<T> = std::result::Result<T, Error>;

/// Entrypoint interface for communicating with docker daemon
pub struct Docker {
    transport: Transport,
}

/// Interface for accessing and manipulating a named docker image
pub struct Image<'a, 'b> {
    docker: &'a Docker,
    name: &'b str,
}

impl<'a, 'b> Image<'a, 'b> {
    /// Exports an interface for operations that may be performed against a named image
    pub fn new(docker: &'a Docker, name: &'b str) -> Image<'a, 'b> {
        Image {
            docker: docker,
            name: name,
        }
    }

    /// Inspects a named image's details
    pub fn inspect(&self) -> Result<ImageDetails> {
        let raw = try!(self.docker.get(&format!("/images/{}/json", self.name)[..]));
        Ok(try!(json::decode::<ImageDetails>(&raw)))
    }

    /// Lists the history of the images set of changes
    pub fn history(&self) -> Result<Vec<History>> {
        let raw = try!(self.docker.get(&format!("/images/{}/history", self.name)[..]));
        Ok(try!(json::decode::<Vec<History>>(&raw)))
    }

    /// Delete's an image
    pub fn delete(&self) -> Result<Vec<Status>> {
        let raw = try!(self.docker.delete(&format!("/images/{}", self.name)[..]));
        Ok(match try!(Json::from_str(&raw)) {
               Json::Array(ref xs) => {
                   xs.iter().map(|j| {
                       let obj = j.as_object().expect("expected json object");
                       obj.get("Untagged")
                          .map(|sha| {
                              Status::Untagged(sha.as_string()
                                                  .expect("expected Untagged to be a string")
                                                  .to_owned())
                          })
                          .or(obj.get("Deleted")
                                 .map(|sha| {
                                     Status::Deleted(sha.as_string()
                                                        .expect("expected Deleted to be a string")
                                                        .to_owned())
                                 }))
                          .expect("expected Untagged or Deleted")
                   })
               }
               _ => unreachable!(),
           }
           .collect())
    }

    /// Export this image to a tarball
    pub fn export(&self) -> Result<Box<Read>> {
        self.docker.stream_get(&format!("/images/{}/get", self.name)[..])
    }
}

/// Interface for docker images
pub struct Images<'a> {
    docker: &'a Docker,
}

impl<'a> Images<'a> {
    /// Exports an interface for interacting with docker images
    pub fn new(docker: &'a Docker) -> Images<'a> {
        Images { docker: docker }
    }

    /// Builds a new image build by reading a Dockerfile in a target directory
    pub fn build(&self, opts: &BuildOptions) -> Result<Box<Iterator<Item = BuildOutput>>> {
        let mut path = vec!["/build".to_owned()];
        if let Some(query) = opts.serialize() {
            path.push(query)
        }

        let mut bytes = vec![];

        try!(tarball::dir(&mut bytes, &opts.path[..]));

        let raw = try!(self.docker.stream_post(&path.join("?"),
                                               Some((Body::BufBody(&bytes[..], bytes.len()),
                                                     tar()))));
        let it = jed::Iter::new(raw).into_iter().map(|j| {
            // fixme: better error handling
            debug!("{:?}", j);
            let obj = j.as_object().expect("expected json object");
            obj.get("stream")
               .map(|stream| {
                   BuildOutput::Stream(stream.as_string()
                                             .expect("expected stream to be a string")
                                             .to_owned())
               })
               .or(obj.get("error")
                      .map(|err| {
                          BuildOutput::Err(err.as_string()
                                              .expect("expected error to be a string")
                                              .to_owned())
                      }))
               .expect("expected build output stream or error")
        });
        Ok(Box::new(it))
    }

    /// Lists the docker images on the current docker host
    pub fn list(&self, opts: &ImageListOptions) -> Result<Vec<ImageRep>> {
        let mut path = vec!["/images/json".to_owned()];
        if let Some(query) = opts.serialize() {
            path.push(query);
        }
        let raw = try!(self.docker.get(&path.join("?")));
        Ok(try!(json::decode::<Vec<ImageRep>>(&raw)))
    }

    /// Returns a reference to a set of operations available for a named image
    pub fn get(&'a self, name: &'a str) -> Image {
        Image::new(self.docker, name)
    }

    /// Search for docker images by term
    pub fn search(&self, term: &str) -> Result<Vec<SearchResult>> {
        let query = form_urlencoded::serialize(vec![("term", term)]);
        let raw = try!(self.docker.get(&format!("/images/search?{}", query)[..]));
        Ok(try!(json::decode::<Vec<SearchResult>>(&raw)))
    }

    /// Pull and create a new docker images from an existing image
    pub fn pull(&self, opts: &PullOptions) -> Result<Box<Iterator<Item = PullOutput>>> {
        let mut path = vec!["/images/create".to_owned()];
        if let Some(query) = opts.serialize() {
            path.push(query);
        }
        let raw = try!(self.docker
                           .stream_post(&path.join("?"), None as Option<(&'a str, ContentType)>));
        let it = jed::Iter::new(raw).into_iter().map(|j| {
            // fixme: better error handling
            debug!("{:?}", j);
            let s = json::encode(&j).unwrap();
            json::decode::<PullInfo>(&s)
                .map(|info| {
                    PullOutput::Status {
                        id: info.id,
                        status: info.status,
                        progress: info.progress,
                        progress_detail: info.progressDetail,
                    }
                })
                .ok()
                .or(j.as_object()
                     .expect("expected json object")
                     .get("error")
                     .map(|err| {
                         PullOutput::Err(err.as_string()
                                            .expect("expected error to be a string")
                                            .to_owned())
                     }))
                .expect("expected pull status or error")
        });
        Ok(Box::new(it))
    }

    /// exports a collection of named images,
    /// either by name, name:tag, or image id, into a tarball
    pub fn export(&self, names: Vec<&str>) -> Result<Box<Read>> {
        let params = names.iter()
                          .map(|n| ("names", *n))
                          .collect::<Vec<(&str, &str)>>();
        let query = form_urlencoded::serialize(params);
        self.docker.stream_get(&format!("/images/get?{}", query)[..])
    }

    // pub fn import(self, tarball: Box<Read>) -> Result<()> {
    //  self.docker.post
    // }
}

/// Interface for accessing and manipulating a docker container
pub struct Container<'a, 'b> {
    docker: &'a Docker,
    id: &'b str,
}

impl<'a, 'b> Container<'a, 'b> {
    /// Exports an interface exposing operations against a container instance
    pub fn new(docker: &'a Docker, id: &'b str) -> Container<'a, 'b> {
        Container {
            docker: docker,
            id: id,
        }
    }

    /// a getter for the container id
    pub fn id(&self) -> &str { &self.id }

    /// Inspects the current docker container instance's details
    pub fn inspect(&self) -> Result<ContainerDetails> {
        let raw = try!(self.docker.get(&format!("/containers/{}/json", self.id)[..]));
        Ok(try!(json::decode::<ContainerDetails>(&raw)))
    }

    /// Returns a `top` view of information about the container process
    pub fn top(&self, psargs: Option<&str>) -> Result<Top> {
        let mut path = vec![format!("/containers/{}/top", self.id)];
        if let Some(ref args) = psargs {
            let encoded = form_urlencoded::serialize(vec![("ps_args", args)]);
            path.push(encoded)
        }
        let raw = try!(self.docker.get(&path.join("?")));

        Ok(try!(json::decode::<Top>(&raw)))
    }

    /// Returns a stream of logs emitted but the container instance
    pub fn logs(&self, opts: &LogsOptions) -> Result<Box<Read>> {
        let mut path = vec![format!("/containers/{}/logs", self.id)];
        if let Some(query) = opts.serialize() {
            path.push(query)
        }
        self.docker.stream_get(&path.join("?"))
    }

    /// Returns a set of changes made to the container instance
    pub fn changes(&self) -> Result<Vec<Change>> {
        let raw = try!(self.docker.get(&format!("/containers/{}/changes", self.id)[..]));
        Ok(try!(json::decode::<Vec<Change>>(&raw)))
    }

    /// Exports the current docker container into a tarball
    pub fn export(&self) -> Result<Box<Read>> {
        self.docker.stream_get(&format!("/containers/{}/export", self.id)[..])
    }

    /// Returns a stream of stats specific to this container instance
    pub fn stats(&self) -> Result<Box<Iterator<Item = Stats>>> {
        let raw = try!(self.docker.stream_get(&format!("/containers/{}/stats", self.id)[..]));
        let it = jed::Iter::new(raw).into_iter().map(|j| {
            // fixme: better error handling
            debug!("{:?}", j);
            let s = json::encode(&j).unwrap();
            json::decode::<Stats>(&s).unwrap()
        });
        Ok(Box::new(it))
    }

    /// Start the container instance
    pub fn start(&'a self) -> Result<()> {
        self.docker
            .post(&format!("/containers/{}/start", self.id)[..],
                  None as Option<(&'a str, ContentType)>)
            .map(|_| ())
    }

    /// Stop the container instance
    pub fn stop(&self, wait: Option<Duration>) -> Result<()> {
        let mut path = vec![format!("/containers/{}/stop", self.id)];
        if let Some(w) = wait {
            let encoded = form_urlencoded::serialize(vec![("t", w.as_secs().to_string())]);
            path.push(encoded)
        }
        self.docker.post(&path.join("?"), None as Option<(&'a str, ContentType)>).map(|_| ())
    }

    /// Restart the container instance
    pub fn restart(&self, wait: Option<Duration>) -> Result<()> {
        let mut path = vec![format!("/containers/{}/restart", self.id)];
        if let Some(w) = wait {
            let encoded = form_urlencoded::serialize(vec![("t", w.as_secs().to_string())]);
            path.push(encoded)
        }
        self.docker.post(&path.join("?"), None as Option<(&'a str, ContentType)>).map(|_| ())
    }

    /// Kill the container instance
    pub fn kill(&self, signal: Option<&str>) -> Result<()> {
        let mut path = vec![format!("/containers/{}/kill", self.id)];
        if let Some(sig) = signal {
            let encoded = form_urlencoded::serialize(vec![("signal", sig.to_owned())]);
            path.push(encoded)
        }
        self.docker.post(&path.join("?"), None as Option<(&'a str, ContentType)>).map(|_| ())
    }

    /// Rename the container instance
    pub fn rename(&self, name: &str) -> Result<()> {
        let query = form_urlencoded::serialize(vec![("name", name)]);
        self.docker
            .post(&format!("/containers/{}/rename?{}", self.id, query)[..],
                  None as Option<(&'a str, ContentType)>)
            .map(|_| ())
    }

    /// Pause the container instance
    pub fn pause(&self) -> Result<()> {
        self.docker
            .post(&format!("/containers/{}/pause", self.id)[..],
                  None as Option<(&'a str, ContentType)>)
            .map(|_| ())
    }

    /// Unpause the container instance
    pub fn unpause(&self) -> Result<()> {
        self.docker
            .post(&format!("/containers/{}/unpause", self.id)[..],
                  None as Option<(&'a str, ContentType)>)
            .map(|_| ())
    }

    /// Wait until the container stops
    pub fn wait(&self) -> Result<Exit> {
        let raw = try!(self.docker.post(&format!("/containers/{}/wait", self.id)[..],
                                        None as Option<(&'a str, ContentType)>));
        Ok(try!(json::decode::<Exit>(&raw)))
    }

    /// Delete the container instance
    /// use remove instead to use the force/v options
    pub fn delete(&self) -> Result<()> {
        self.docker.delete(&format!("/containers/{}", self.id)[..]).map(|_| ())
    }

    /// Delete the container instance (todo: force/v)
    pub fn remove(&self, opts: RmContainerOptions) -> Result<()> {
        let mut path = vec![format!("/containers/{}", self.id)];
        if let Some(query) = opts.serialize() {
            path.push(query)
        }
        try!(self.docker.delete(&path.join("?")));
        Ok(())
    }

    // todo attach, attach/ws, copy, archive
}

/// Interface for docker containers
pub struct Containers<'a> {
    docker: &'a Docker,
}

impl<'a> Containers<'a> {
    /// Exports an interface for interacting with docker containers
    pub fn new(docker: &'a Docker) -> Containers<'a> {
        Containers { docker: docker }
    }

    /// Lists the container instances on the docker host
    pub fn list(&self, opts: &ContainerListOptions) -> Result<Vec<ContainerRep>> {
        let mut path = vec!["/containers/json".to_owned()];
        if let Some(query) = opts.serialize() {
            path.push(query)
        }
        let raw = try!(self.docker.get(&path.join("?")));
        Ok(try!(json::decode::<Vec<ContainerRep>>(&raw)))
    }

    /// Returns a reference to a set of operations available to a specific container instance
    pub fn get(&'a self, name: &'a str) -> Container {
        Container::new(self.docker, name)
    }

    /// Returns a builder interface for creating a new container instance
    pub fn create(&'a self, opts: &ContainerOptions) -> Result<ContainerCreateInfo> {
        let data = try!(opts.serialize());
        let mut bytes = data.as_bytes();
        let raw = try!(self.docker.post("/containers/create",
                                        Some((&mut bytes, ContentType::json()))));
        Ok(try!(json::decode::<ContainerCreateInfo>(&raw)))
    }
}

// https://docs.docker.com/reference/api/docker_remote_api_v1.17/
impl Docker {
    /// constructs a new Docker instance for a docker host listening at a url specified by an env var `DOCKER_HOST`,
    /// falling back on unix:///var/run/docker.sock
    pub fn new() -> Docker {
        let fallback: std::result::Result<String, VarError> = Ok("unix:///var/run/docker.sock"
                                                                     .to_owned());
        let host = env::var("DOCKER_HOST")
                       .or(fallback)
                       .map(|h| {
                           Url::parse(&h)
                               .ok()
                               .expect("invalid url")
                       })
                       .ok()
                       .expect("expected host");
        Docker::host(host)
    }

    /// constructs a new Docker instance for docker host listening at the given host url
    pub fn host(host: Url) -> Docker {
        let domain = match host.scheme_data {
            SchemeData::NonRelative(s) => s,
            SchemeData::Relative(RelativeSchemeData { host, .. }) => {
                match host {
                    Host::Domain(s) => s,
                    Host::Ipv6(a) => a.to_string(),
                    Host::Ipv4(a) => a.to_string(),
                }
            }
        };
        match &host.scheme[..] {
            "unix" => {
                Docker {
                    transport: Transport::Unix {
                        client: Client::with_connector(UnixSocketConnector),
                        path: domain,
                    },
                }
            }
            _ => {
                let client = if let Some(ref certs) = env::var("DOCKER_CERT_PATH").ok() {
                    // fixme: don't unwrap before you know what's in the box
                    // https://github.com/hyperium/hyper/blob/master/src/net.rs#L427-L428
                    let mut ssl_ctx = SslContext::new(SslMethod::Sslv23).unwrap();
                    ssl_ctx.set_cipher_list("DEFAULT").unwrap();
                    let cert = &format!("{}/cert.pem", certs);
                    let key = &format!("{}/key.pem", certs);
                    let _ = ssl_ctx.set_certificate_file(&Path::new(cert), X509FileType::PEM);
                    let _ = ssl_ctx.set_private_key_file(&Path::new(key), X509FileType::PEM);
                    if let Some(_) = env::var("DOCKER_TLS_VERIFY").ok() {
                        let ca = &format!("{}/ca.pem", certs);
                        let _ = ssl_ctx.set_CA_file(&Path::new(ca));
                    };
                    Client::with_connector(HttpsConnector::new(Openssl {
                        context: Arc::new(ssl_ctx),
                    }))
                } else {
                    Client::new()
                };
                Docker {
                    transport: Transport::Tcp {
                        client: client,
                        host: format!("https:{}", domain.to_owned()),
                    },
                }
            }
        }
    }

    /// Exports an interface for interacting with docker images
    pub fn images<'a>(&'a self) -> Images {
        Images::new(self)
    }

    /// Exports an interface for interacting with docker containers
    pub fn containers<'a>(&'a self) -> Containers {
        Containers::new(self)
    }

    /// Returns version information associated with the docker daemon
    pub fn version(&self) -> Result<Version> {
        let raw = try!(self.get("/version"));
        Ok(try!(json::decode::<Version>(&raw)))
    }

    /// Returns information associated with the docker daemon
    pub fn info(&self) -> Result<Info> {
        let raw = try!(self.get("/info"));
        Ok(try!(json::decode::<Info>(&raw)))
    }

    /// Returns a simple ping response indicating the docker daemon is accessible
    pub fn ping(&self) -> Result<String> {
        self.get("/_ping")
    }

    /// Returns an interator over streamed docker events
    pub fn events(&self, opts: &EventsOptions) -> Result<Box<Iterator<Item = Event>>> {
        let mut path = vec!["/events".to_owned()];
        if let Some(query) = opts.serialize() {
            path.push(query);
        }
        let raw = try!(self.stream_get(&path.join("?")[..]));
        let it = jed::Iter::new(raw).into_iter().map(|j| {
            debug!("{:?}", j);
            // fixme: better error handling
            let s = json::encode(&j).unwrap();
            json::decode::<Event>(&s).unwrap()
        });
        Ok(Box::new(it))
    }

    fn get<'a>(&self, endpoint: &str) -> Result<String> {
        self.transport.request(Method::Get,
                               endpoint,
                               None as Option<(&'a str, ContentType)>)
    }

    fn post<'a, B>(&'a self, endpoint: &str, body: Option<(B, ContentType)>) -> Result<String>
        where B: Into<Body<'a>>
    {
        self.transport.request(Method::Post, endpoint, body)
    }

    fn delete<'a>(&self, endpoint: &str) -> Result<String> {
        self.transport.request(Method::Delete,
                               endpoint,
                               None as Option<(&'a str, ContentType)>)
    }

    fn stream_post<'a, B>(&'a self,
                          endpoint: &str,
                          body: Option<(B, ContentType)>)
                          -> Result<Box<Read>>
        where B: Into<Body<'a>>
    {
        self.transport.stream(Method::Post, endpoint, body)
    }

    fn stream_get<'a>(&self, endpoint: &str) -> Result<Box<Read>> {
        self.transport.stream(Method::Get,
                              endpoint,
                              None as Option<(&'a str, ContentType)>)
    }
}
