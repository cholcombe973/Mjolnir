// use futures;
// use futures::future::Future;
// use futures::Future;
use std::sync::Arc;
use std::time::Duration;
use std::thread;

use hostname::get_hostname;
// use hyper;
// use hyper::header::ContentLength;
// use hyper::server::{Http, Request, Response, Service};

// use hyper::Client;

// use tokio_core::reactor::Core;

use mjolnir_api::{Operation, OperationType as OpType, Register, parse_from_bytes};

use protobuf::Message as ProtobufMsg;

use zmq::{Message, Result as ZmqResult};

use config::{Config, Master};
use server::{connect, server_pubkey, zmq_listen};

#[derive(Clone)]
pub struct Agent {
    masters: Arc<Vec<Master>>,
    pubkey: String,
    config: Config,
}

impl Agent {
    pub fn bind(config: Config, masters: Vec<Master>) -> ZmqResult<()> {

        let background_config = config.clone();
        let agent = Agent {
            masters: Arc::new(masters),
            pubkey: server_pubkey(&config).into(),
            config: config,
        };

        let _ = agent.register();
        let ping_duration = Duration::from_millis(500);
        let masters = agent.masters.clone();
        thread::spawn(move|| {
            let server_pubkey = server_pubkey(&background_config);
            loop {
                for master in masters.iter() {
                    match connect(&master.ip, master.zmq_port, &server_pubkey){
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
            
                thread::sleep(ping_duration);
            }
        });
        let _ = agent.listen();
        Ok(())
    }

    fn listen(&self) -> ZmqResult<()> {
        zmq_listen(
            &self.config,
            Box::new(|operation, responder| {
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
                    _ => {
                        println!("Not quite handling {:?} yet", operation);
                    }
                }
                Ok(())
            }),
        )
    }

    fn register(&self) -> ZmqResult<()> {
        // register with the master!
        let master = &self.masters[0];
        let socket = match connect(&master.ip, master.zmq_port, &self.pubkey) {
            Ok(s) => s,
            Err(e) => {
                println!("Error connecting to socket: {:?}", e);
                return Err(e);
            }
        };

        let mut o = Operation::new();
        println!("Creating  operation request");
        o.set_operation_type(OpType::REGISTER);
        let register = Register::new(
            self.config.my_ip.clone(),
            self.config.zmq_port,
            get_hostname().unwrap(),
        );
        o.set_register(register.into());
        let encoded = o.write_to_bytes().unwrap();
        let msg = Message::from_slice(&encoded)?;
        println!("Sending message");
        socket.send_msg(msg, 0)?;
        match socket.recv_bytes(0) {
            Ok(msg) => {
                println!("Got msg len: {}", msg.len());
                println!("Parsing msg {:?} as hex", msg);
                let operation = match parse_from_bytes::<Operation>(&msg) {
                    Ok(bytes) => bytes,
                    Err(e) => {
                        println!("Failed to parse_from_bytes {:?}.  Ignoring request", e);
                        // TODO: Proper error handling
                        return Ok(());
                    }
                };
                println!("Operation is: {:?}", operation);
                match operation.get_operation_type() {
                    OpType::ACK => {
                        println!("got our ACK!");
                    }
                    _ => {
                        println!("Not quite handling {:?} yet", operation);
                    }
                }
            }
            Err(e) => {
                println!("Failed to recieve bytes: {:?}", e);
                return Err(e);
            }
        }
        Ok(())
    }
}