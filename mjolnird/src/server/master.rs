use std::time::{Instant, Duration};
use std::fs::read_dir;
use std::net::IpAddr;
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::thread;

use futures;
// use futures::future::Future;
use futures::{Future, Stream};

use hyper;
use hyper::server::{Http, Request, Response, Service};

use hyper::{Body, Chunk, Method, StatusCode};

use zmq::{Message, Result as ZmqResult};

use protobuf::Message as ProtobufMsg;

use mjolnir::Pipeline;
use mjolnir_api::{Operation, OperationType as OpType, PluginEntry, Register};
use server::{zmq_listen, connect, server_pubkey, load_pipeline};
use config::Config;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_process_webhook() {
        let plugin = PluginEntry {
            name: "test-name".into(),
            author: "test author".into(),
            version: "test version".into(),
            webhook: true,
            alerts: vec![],
            remediations: vec![],
            path: PathBuf::from("/bin/echo"),
        };

        let body = process_webhook(plugin, "test".into());
        assert_eq!(body, "plugin=test-name body=test\n")
    }

    #[test]
    fn it_validates_pipelines() {
        let args = Config::matches().get_matches_from(vec![
            "mjolnird",
            "--bind=192.168.0.101:11011",
            "--config=../examples/configs",
            // "--plugins=/usr/local/share",
            "--ip=127.0.0.1",
            "master",
        ]);
        let config = Config::from_args(args);
        let mut master = Master::default()
            .with_plugin_path(config.plugin_path.clone());
        master.plugins.push(PluginEntry {
            name: "clean_disk".into(),
            author: "test author".into(),
            version: "test version".into(),
            webhook: true,
            alerts: vec![],
            remediations: vec![],
            path: PathBuf::from("/bin/echo"),
        });
        master = master
            .load_pipelines(&config);
        assert!(master.pipelines.len() == 1);
    }
}

#[derive(Clone, Debug)]
struct Agent {
    ip: IpAddr,
    hostname: String,
    port: u16,
    last_seen: Instant,
}

impl PartialEq for Agent {
    fn eq(&self, other: &Agent) -> bool {
        self.ip == other.ip && self.hostname == other.hostname && self.port == other.port
    }
}

#[derive(Clone, Debug, Default)]
pub struct Master {
    agents: Arc<Mutex<Vec<Agent>>>,
    plugins: Vec<PluginEntry>,
    plugin_path: Option<PathBuf>,
    pipelines: Vec<Pipeline>,
}

impl Service for Master {
    // boilerplate hooking up hyper's server types
    type Request = Request;
    // type Response = Response;
    type Response = Response<Box<Stream<Item = Chunk, Error = Self::Error>>>;
    type Error = hyper::Error;
    // The future representing the eventual Response your call will
    // resolve to. This can change to whatever Future you need.
    type Future = Box<Future<Item = Self::Response, Error = Self::Error>>;

    fn call(&self, req: Request) -> Self::Future {
        self.route(req)
    }
}

fn hello(
    req: Request,
) -> Box<
    Future<
        Item = Response<Box<Stream<Item = Chunk, Error = hyper::Error>>>,
        Error = hyper::Error,
    >,
> {
    let phrase = "Hello, from Master";
    let mut response = Response::new();
    println!("Received request: {} {}", req.method(), req.path());
    let body: Box<Stream<Item = _, Error = _>> = Box::new(Body::from(phrase));
    response.set_body(body);
    // response.set_body(phrase);
    Box::new(futures::future::ok(response))
}

fn process_webhook(hook: PluginEntry, body: String) -> String {
    println!("Hook is: {:?}", hook);
    let mut cmd = Command::new(hook.path);
    cmd.arg(format!("plugin={}", hook.name));
    cmd.arg(format!("body={}", body));
    if let Ok(output) = cmd.output() {
        match String::from_utf8(output.stdout) {
            Ok(s) => s,
            Err(_) => "".into(),
        }
    } else {
        "Ok".into()
    }
}

impl Master {
    fn webhook(
        &self,
        name: &str,
        req: Request,
    ) -> Box<
        Future<
            Item = Response<Box<Stream<Item = Chunk, Error = hyper::Error>>>,
            Error = hyper::Error,
        >,
    > {
        println!("Responding to webook {} at {}", name, req.path());
        // let plugins = plugins.clone();
        let hook = self.plugins
            .iter()
            .filter(|wh| wh.webhook)
            .filter(|wh| wh.name == name)
            .nth(0)
            .map(|p| p.clone());
        // let hook: Option<PluginEntry> = *hook.clone();

        Box::new(req.body().concat2().map(move |body| {
            // let plugins = plugins.clone();
            let body: Box<Stream<Item = _, Error = _>> = if let Some(hook) = hook {
                match String::from_utf8(body.to_vec()) {
                    Ok(s) => Box::new(Body::from(process_webhook(hook, s))),
                    Err(_) => Box::new(Body::from("Invalid Body")),
                }
            // println!("Body is: {:?}", body);
            // cmd.arg(hook.name);
            // hook.args.each
            } else {
                Box::new(Body::from("Unknown Webhook"))
            };
            let mut response: Response<Box<Stream<Item = Chunk, Error = hyper::Error>>> =
                Response::new();

            response.set_body(body);
            response
        }))
    }

    pub fn bind(config: Config) -> ZmqResult<()> {
        let master = Master::default()
            .with_plugin_path(config.plugin_path.clone())
            .load_plugins()
            .load_pipelines(&config);

        let http_config = config.clone();

        // OH MY GOD THE PAIN TO KEEP THE RIGHT THING ALIVE
        let closure_master = master.clone();
        thread::spawn(move || {
            let master_server = move || Ok(closure_master.clone());
            let server = Http::new().bind(&http_config.bind_address, master_server)?;
            server.run()
        });
        let background_agents = master.agents.clone();
        let background_config = config.clone();
        let ping_duration = Duration::from_millis(500);
        thread::spawn(move|| {
            let server_pubkey = server_pubkey(&background_config);
            loop {
                if let Ok(agents) = background_agents.try_lock() {
                    for agent in agents.iter() {
                        match connect(&agent.hostname, agent.port, &server_pubkey){
                            Ok(socket) => {
                                let mut o = Operation::new();
                                println!("Creating PING");
                                o.set_operation_type(OpType::PING);

                                let encoded = o.write_to_bytes().unwrap();
                                let msg = Message::from_slice(&encoded).unwrap();
                                match socket.send_msg(msg, 0) {
                                    Ok(_s) => {},
                                    Err(e) => println!("Problem snding ping: {:?}", e)
                                }
                            }
                            Err(e) => println!("problem connecting to socket: {:?}", e),
                        }
                    }
                } else {
                    println!("Failed to lock agents mutex for PING");
                }
            
                thread::sleep(ping_duration);
            }
        });
        let _ = master.setup_zmq(&config)?;
        thread::park();
        Ok(())
    }

    fn setup_zmq(&self, config: &Config) -> ZmqResult<()> {
        let agents: Arc<Mutex<Vec<Agent>>> = self.agents.clone();
        zmq_listen(
            config,
            Box::new(move |operation, responder| {
                // let agents_arc = agents.clone();
                match operation.get_operation_type() {
                    OpType::PING => {
                        let mut o = Operation::new();
                        println!("Creating pong");
                        o.set_operation_type(OpType::PONG);
                        o.set_ping_id(operation.get_ping_id());
                        let encoded = o.write_to_bytes().unwrap();
                        let msg = Message::from_slice(&encoded)?;
                        responder.send_msg(msg, 0)?;
                    }
                    OpType::REGISTER => {
                        let mut o = Operation::new();
                        println!("Creating ack");
                        o.set_operation_type(OpType::ACK);

                        let encoded = o.write_to_bytes().unwrap();
                        let msg = Message::from_slice(&encoded)?;
                        responder.send_msg(msg, 0)?;
                        let mut agents = agents.lock().unwrap();
                        let register: Register = operation.get_register().clone().into();
                        let agent = Agent {
                            ip: register.ip,
                            hostname: register.hostname,
                            port: register.port,
                            last_seen: Instant::now(),
                        };
                        let mut updated = false;
                        {
                            let known = agents.iter_mut().filter(|a| **a == agent).nth(0);
                            if let Some(known_agent) = known {
                                known_agent.last_seen = agent.last_seen;
                                updated = true;
                            }
                        }
                        if !updated {
                            agents.push(agent);
                        }

                        println!("#{} Agents", agents.len());
                    }
                    _ => {
                        println!("Not quite handling {:?} yet", operation);
                    }
                }
                Ok(())
            }),
        )
    }

    fn route(
        &self,
        req: Request,
    ) -> Box<
        Future<
            Item = Response<Box<Stream<Item = Chunk, Error = hyper::Error>>>,
            Error = hyper::Error,
        >,
    > {
        match (req.method(), req.path()) {
            (&Method::Post, _) => {
                let path = req.path().to_string();
                let mut parts = path.split("/").clone();
                let _ = parts.next();
                match (parts.next(), parts.next()) {
                    (Some("webhook"), Some(name)) => self.webhook(name, req),
                    (_first, _second) => hello(req),
                }
            }
            _ => {
                let mut response = Response::new();
                println!("Received request: {} {}", req.method(), req.path());
                response.set_status(StatusCode::NotFound);
                Box::new(futures::future::ok(response))
            }
        }
    }

    fn with_plugin_path(mut self, path: PathBuf) -> Self {
        self.plugin_path = Some(path);
        self
    }

    fn load_plugins(mut self) -> Self {
        let path = self.plugin_path.clone();
        if let Some(ref path) = path {
            let mut plugins = vec![];
            if let Ok(dir) = read_dir(path) {
                for file in dir {
                    if let Ok(file) = file {
                        if let Ok(output) = Command::new(file.path()).output() {
                            if let Ok(plugin) = PluginEntry::try_from(
                                &output.stdout,
                                file.path(),
                            )
                            {
                                if !plugins.contains(&plugin) {
                                    plugins.push(plugin);
                                }
                            } else {
                                println!("Had a problem loading pluginn at {:?}", file.path());
                            }
                        }
                    }
                }
            }
            self.plugins = plugins;
        }
        self
    }

    fn load_pipelines(mut self, config: &Config) -> Self {
        let pipelines = load_pipeline(config);

        match self.validate_pipelines(&pipelines) {
            Ok(()) => {},
            Err(e) => panic!("Couldn't load plugin that matches your pipeline: {:?}", e),
        }

        self.pipelines = pipelines;
        self
    }

    fn validate_pipelines(&self, pipelines: &Vec<Pipeline>) -> Result<(), String> {
        for pipeline in pipelines {
            println!("Validating we have a plugin configured for '{}'", pipeline.action.plugin);
            if !self.plugins.iter().any(|p| p.name == pipeline.action.plugin) {
                return Err(format!("{} has no matching plugin", pipeline.action.plugin));
            }
        }
        Ok(())
    }
}