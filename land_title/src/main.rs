#![feature(custom_attribute)]
#![feature(type_ascription)]
#![feature(custom_derive)]
#![feature(plugin)]

#[macro_use]
extern crate rustless;
extern crate jsonway;
extern crate iron;
extern crate hyper;
extern crate valico;
extern crate env_logger;
extern crate clap;
extern crate serde;
extern crate serde_json;
extern crate time;
extern crate rand;
// extern crate hex;

extern crate exonum;
extern crate blockchain_explorer;
extern crate land_title;

// use land_title::cors::CORS;
// use iron::method::Method;

use std::net::SocketAddr;
use std::thread;
use std::default::Default;

use clap::{Arg, App};
use rustless::json::ToJson;
use rustless::{Application, Api, Nesting, Versioning, Response, Client, ErrorResponse};
use rustless::batteries::cookie::{Cookie, CookieExt, CookieJar};
use rustless::batteries::swagger;
use rustless::errors;
use valico::json_dsl;
use hyper::status::StatusCode;
use serde_json::value::from_value;

use exonum::blockchain::GenesisConfig;
use exonum::node::{Node, NodeConfig, TxSender, NodeChannel};
use exonum::storage::Database;
use exonum::storage::{Result as StorageResult, Error as StorageError};
use exonum::crypto::{gen_keypair, PublicKey, SecretKey, HexValue, Hash, FromHexError};
use exonum::messages::Message;
use blockchain_explorer::ValueNotFound;
use blockchain_explorer::helpers::{GenerateCommand, RunCommand, DatabaseType};

use land_title::GeoPoint;
use land_title::{ObjectsBlockchain, ObjectTx, TxCreateOwner, TxCreateObject, TxTransferObject,
                 TxRemoveObject, TxRestoreObject, TxRegister};
use land_title::api::{ObjectsApi, NewObject};

pub type Channel<B> = TxSender<B, NodeChannel<B>>;

fn save_user(storage: &mut CookieJar, public_key: &PublicKey, secret_key: &SecretKey) {
    let p = storage.permanent();
    let add_cookie = |name: &str, value| {
        let mut cookie = Cookie::new(name.to_string(), value);
        cookie.path = Some("/".to_string());
        p.add(cookie)
    };

    add_cookie("public_key", public_key.to_hex());
    add_cookie("secret_key", secret_key.to_hex());
}

fn load_hex_value_from_cookie<'a>(storage: &'a CookieJar, key: &str) -> StorageResult<Vec<u8>> {
    if let Some(cookie) = storage.find(key) {
        println!("{}", cookie);
        if let Ok(value) = HexValue::from_hex(cookie.value) {
            return Ok(value);
        }
    }
    Err(StorageError::new(format!("Unable to find value with given key {}", key)))
}

fn load_user(storage: &CookieJar) -> StorageResult<(PublicKey, SecretKey)> {
    let p = storage.permanent();

    let public_key = PublicKey::from_slice(load_hex_value_from_cookie(&p, "public_key")?.as_ref());
    let secret_key = SecretKey::from_slice(load_hex_value_from_cookie(&p, "secret_key")?.as_ref());

    let public_key = public_key.ok_or(StorageError::new("Unable to read public key"))?;
    let secret_key = secret_key.ok_or(StorageError::new("Unable to read secret key"))?;
    Ok((public_key, secret_key))
}

fn send_transaction<'a, D: Database>(tx: ObjectTx,
                                     _: &Client<'a>,
                                     ch: Channel<ObjectsBlockchain<D>>)
                                     -> String {

    let tx_hash = tx.hash().to_hex();
    let result = ch.send(tx);
    println!("{:?}", result);
    tx_hash
}
fn send_tx<'a, D: Database>(tx: ObjectTx,
                            client: Client<'a>,
                            ch: Channel<ObjectsBlockchain<D>>)
                            -> Result<Client<'a>, ErrorResponse> {
    let tx_hash = send_transaction(tx, &client, ch);
    let json = &jsonway::object(|json| json.set("tx_hash", tx_hash)).unwrap();
    client.json(json)
}

fn run_node<D: Database>(blockchain: ObjectsBlockchain<D>,
                         node_cfg: NodeConfig,
                         port: Option<u16>,
                         origin: Option<String>) {
    if let Some(port) = port {

        let mut node = Node::new(blockchain.clone(), node_cfg.clone());

        let channel = node.channel();

        let api_thread = thread::spawn(move || {
            let channel = channel.clone();
            let blockchain = blockchain.clone();

            let api = Api::build(move |api| {
                // Specify API version
                api.version("v1", Versioning::Path);
                api.prefix("registry");

                api.error_formatter(|err, _media| {
                    let body;
                    let code;
                    if let Some(e) = err.downcast::<StorageError>() {
                        code = StatusCode::InternalServerError;
                        body = format!("An error in backend occured: {}", e);
                    } else if let Some(e) = err.downcast::<errors::NotMatch>() {
                        code = StatusCode::NotFound;
                        body = e.to_string();
                    } else if let Some(e) = err.downcast::<errors::Validation>() {
                        code = StatusCode::BadRequest;
                        body = e.to_string();
                    } else if let Some(e) = err.downcast::<ValueNotFound>() {
                        code = StatusCode::NotFound;
                        body = e.to_string();
                    } else if let Some(e) = err.downcast::<FromHexError>() {
                        code = StatusCode::BadRequest;
                        body = e.to_string();
                    } else {
                        code = StatusCode::NotImplemented;
                        body = format!("Unspecified error: {:?}", err);
                    }

                    let json = &jsonway::object(|json| json.set("message", body)).unwrap();
                    Some(Response::from_json(code, &json))
                });

                blockchain_explorer_api(api, blockchain.clone(), node_cfg.genesis.clone());
                land_titles_api(api, blockchain.clone(), channel.clone());
                api.mount(swagger::create_api("docs"));
            });

            let listen_address: SocketAddr = format!("127.0.0.1:{}", port).parse().unwrap();

            let mut app = Application::new(api);

            swagger::enable(&mut app,
                            swagger::Spec {
                                info: swagger::Info {
                                    title: "LandTitles API".to_string(),
                                    description: Some("Simple API to demonstration".to_string()),
                                    contact: Some(swagger::Contact {
                                        name: "Aleksandr Marinenko".to_string(),
                                        url: Some("aleksandr.marinenko@xdev.re".to_string()),
                                        ..Default::default()
                                    }),
                                    license: Some(swagger::License {
                                        name: "Demo".to_string(),
                                        url: "http://exonum.com".to_string(),
                                    }),
                                    ..Default::default()
                                },
                                ..Default::default()
                            });

            let mut chain = iron::Chain::new(app);
            let api_key = b"abacabsasdainblabla23nx8Hasojd8";
            let cookie = ::rustless::batteries::cookie::new(api_key);

            let origin_url = {
                if let Some(origin) = origin {
                    origin
                } else {
                    String::from("*")
                }
            };

            println!("LandTitles node server started on {}, allowed origin is {}",
                     listen_address,
                     origin_url);

            // let cors = CORS::new(origin_url, vec![(vec![Method::Get, Method::Post], "owners".to_owned())]);

            chain.link(cookie);
            // chain.link_after(cors);
            iron::Iron::new(chain).http(listen_address).unwrap();
        });

        node.run().unwrap();
        api_thread.join().unwrap();
    } else {
        Node::new(blockchain, node_cfg).run().unwrap();
    }
}

fn blockchain_explorer_api<D: Database>(api: &mut Api,
                                        b1: ObjectsBlockchain<D>,
                                        cfg: GenesisConfig) {
    blockchain_explorer::make_api::<ObjectsBlockchain<D>, ObjectTx>(api, b1, cfg);
}


fn land_titles_api<D: Database>(api: &mut Api,
                                blockchain: ObjectsBlockchain<D>,
                                channel: Channel<ObjectsBlockchain<D>>) {

    api.namespace("obm", move |api| {

        api.options("*",
                    move |endpoint| endpoint.handle(move |client, _| client.empty()));

        let ch = channel.clone();
        api.post("register", move |endpoint| {

            endpoint.params(|params| {
                params.req_typed("name", json_dsl::string());
            });

            endpoint.handle(move |client, params| {

                let name = params.find("name").unwrap().as_str().unwrap();

                let (pub_key, sec_key) = gen_keypair();
                {
                    let mut cookies = client.request.cookies();
                    save_user(&mut cookies, &pub_key, &sec_key);
                }
                let tx = TxRegister::new(&pub_key, &name, &sec_key);
                send_tx(ObjectTx::Register(tx), client, ch.clone())

            })
        });

        let b = blockchain.clone();
        api.get("result/:tx", move |endpoint| {

            endpoint.params(|params| {
                params.req_typed("tx", json_dsl::string());
            });

            endpoint.handle(move |client, params| {

                let (_, _) = {
                    let r = {
                        let cookies = client.request.cookies();
                        load_user(&cookies)
                    };
                    match r {
                        Ok((p, s)) => (p, s),
                        Err(e) => return client.error(e),
                    }
                };

                let hash = params.find("tx").unwrap().as_str().unwrap();
                match Hash::from_hex(hash) {
                    Ok(hash) => {
                        let obm = ObjectsApi::new(b.clone());

                        match obm.result(hash) {
                            Ok(Some(result)) => client.json(&result.to_json()),
                            Ok(None) => {
                                client.error(ValueNotFound::new("Unable to find transaction"))
                            }
                            Err(e) => client.error(e),
                        }
                    }
                    Err(_) => client.error(StorageError::new("Unable to decode transaction hash")),
                }

            })
        });

        let b = blockchain.clone();
        api.get("owners", move |endpoint| {
            endpoint.handle(move |client, _| {
                let obm = ObjectsApi::new(b.clone());
                match obm.owners_list() {
                    Ok(Some(info)) => client.json(&info.to_json()),
                    Ok(None) => client.error(ValueNotFound::new("Unable to find content")),
                    Err(e) => client.error(e),
                }
            })
        });

        let b = blockchain.clone();
        api.get("owners/:id", move |endpoint| {
            endpoint.params(|params| {
                params.req_typed("id", json_dsl::u64());
            });
            endpoint.handle(move |client, params| {

                let id = params.find("id").unwrap().as_u64().unwrap();
                let obm = ObjectsApi::new(b.clone());

                match obm.owner_info(id as u64) {
                    Ok(Some(info)) => client.json(&info.to_json()),
                    Ok(None) => client.error(ValueNotFound::new("Unable to get owner")),
                    Err(e) => client.error(e),
                }
            })
        });

        let b = blockchain.clone();
        api.get("objects/:id", move |endpoint| {
            endpoint.params(|params| {
                params.req_typed("id", json_dsl::u64());
            });
            endpoint.handle(move |client, params| {

                let id = params.find("id").unwrap().as_u64().unwrap();
                let obm = ObjectsApi::new(b.clone());

                match obm.object_info(id as u64) {
                    Ok(Some(info)) => client.json(&info.to_json()),
                    Ok(None) => client.error(ValueNotFound::new("Unable to get owner")),
                    Err(e) => client.error(e),
                }
            })
        });

        let ch = channel.clone();
        api.post("objects/transfer", move |endpoint| {
            endpoint.params(|params| {
                params.req_typed("id", json_dsl::u64());
                params.req_typed("owner_id", json_dsl::u64());
            });
            endpoint.handle(move |client, params| {

                let (pub_key, sec_key) = {
                    let r = {
                        let cookies = client.request.cookies();
                        load_user(&cookies)
                    };
                    match r {
                        Ok((p, s)) => (p, s),
                        Err(e) => return client.error(e),
                    }
                };

                let object_id = params.find("id").unwrap().as_u64().unwrap();
                let owner_id = params.find("owner_id").unwrap().as_u64().unwrap();
                let tx = TxTransferObject::new(&pub_key,
                                               object_id,
                                               owner_id,
                                               land_title::timestamp(),
                                               &sec_key);

                send_tx(ObjectTx::TransferObject(tx), client, ch.clone())
            })
        });

        let ch = channel.clone();
        api.delete("objects/:id", move |endpoint| {
            endpoint.params(|params| {
                params.req_typed("id", json_dsl::u64());
            });
            endpoint.handle(move |client, params| {

                let (pub_key, sec_key) = {
                    let r = {
                        let cookies = client.request.cookies();
                        load_user(&cookies)
                    };
                    match r {
                        Ok((p, s)) => (p, s),
                        Err(e) => return client.error(e),
                    }
                };

                let id = params.find("id").unwrap().as_u64().unwrap();
                let tx = TxRemoveObject::new(&pub_key, id, land_title::timestamp(), &sec_key);

                send_tx(ObjectTx::RemoveObject(tx), client, ch.clone())
            })
        });

        let ch = channel.clone();
        api.post("objects/restore", move |endpoint| {
            endpoint.params(|params| {
                params.req_typed("id", json_dsl::u64());
            });
            endpoint.handle(move |client, params| {

                let (pub_key, sec_key) = {
                    let r = {
                        let cookies = client.request.cookies();
                        load_user(&cookies)
                    };
                    match r {
                        Ok((p, s)) => (p, s),
                        Err(e) => return client.error(e),
                    }
                };

                let id = params.find("id").unwrap().as_u64().unwrap();
                let tx = TxRestoreObject::new(&pub_key, id, land_title::timestamp(), &sec_key);

                send_tx(ObjectTx::RestoreObject(tx), client, ch.clone())
            })
        });



        let ch = channel.clone();
        api.post("owners", move |endpoint| {

            endpoint.params(|params| {
                params.req_typed("firstname", json_dsl::string());
                params.req_typed("lastname", json_dsl::string());
            });

            endpoint.handle(move |client, params| {

                let firstname = params.find("firstname").unwrap().as_str().unwrap();
                let lastname = params.find("lastname").unwrap().as_str().unwrap();

                let (pub_key, sec_key) = {
                    let r = {
                        let cookies = client.request.cookies();
                        load_user(&cookies)
                    };
                    match r {
                        Ok((p, s)) => (p, s),
                        Err(e) => return client.error(e),
                    }
                };

                let tx = TxCreateOwner::new(&pub_key, &firstname, &lastname, &sec_key);

                send_tx(ObjectTx::CreateOwner(tx), client, ch.clone())

            })
        });

        let b = blockchain.clone();
        api.get("objects", move |endpoint| {
            endpoint.handle(move |client, _| {
                let obm = ObjectsApi::new(b.clone());
                match obm.objects_list() {
                    Ok(Some(info)) => client.json(&info.to_json()),
                    Ok(None) => client.error(ValueNotFound::new("Unable to find content")),
                    Err(e) => client.error(e),
                }
            })
        });

        let ch = channel.clone();
        api.post("objects", move |endpoint| {

            endpoint.params(|params| {
                params.req_typed("title", json_dsl::string());
                params.req_nested("points", json_dsl::array(), |params| {
                    params.req_typed("x", json_dsl::f64());
                    params.req_typed("y", json_dsl::f64());
                });
                params.req_typed("owner_id", json_dsl::u64());
                params.req_typed("deleted", json_dsl::boolean());
            });

            endpoint.handle(move |client, params| {
                let object_info = from_value::<NewObject>(params.clone()).unwrap();
                let (pub_key, sec_key) = {
                    let r = {
                        let cookies = client.request.cookies();
                        load_user(&cookies)
                    };
                    match r {
                        Ok((p, s)) => (p, s),
                        Err(e) => return client.error(e),
                    }
                };
                let points = GeoPoint::to_vec(&object_info.points);

                let tx = TxCreateObject::new(&pub_key,
                                             &object_info.title,
                                             &points,
                                             object_info.owner_id,
                                             &sec_key);

                send_tx(ObjectTx::CreateObject(tx), client, ch.clone())

            })

        });

    });
}

fn main() {
    exonum::crypto::init();
    blockchain_explorer::helpers::init_logger().unwrap();

    let app = App::new("Land titles manager api")
        .version(env!("CARGO_PKG_VERSION"))
        .author("Aleksandr M. <aleksandr.marinenko@xdev.re>")
        .about("Demo lt validator node")
        .subcommand(GenerateCommand::new())
        .subcommand(RunCommand::new()
            .arg(Arg::with_name("HTTP_PORT")
                .short("p")
                .long("port")
                .value_name("HTTP_PORT")
                .help("Run http server on given port")
                .takes_value(true))
            .arg(Arg::with_name("ORIGIN")
                .short("o")
                .long("origin")
                .value_name("ORIGIN")
                .help("Set origin for CORS")
                .takes_value(true)));

    let matches = app.get_matches();
    match matches.subcommand() {
        ("generate", Some(matches)) => GenerateCommand::execute(matches),
        ("run", Some(matches)) => {
            let port: Option<u16> = matches.value_of("HTTP_PORT").map(|x| x.parse().unwrap());
            let origin: Option<String> = matches.value_of("ORIGIN").map(|x| x.parse().unwrap());
            let node_cfg = RunCommand::node_config(matches);
            match RunCommand::db(matches) {
                DatabaseType::LevelDB(db) => {
                    run_node(ObjectsBlockchain { db: db }, node_cfg, port, origin)
                }
                DatabaseType::MemoryDB(db) => {
                    run_node(ObjectsBlockchain { db: db }, node_cfg, port, origin)
                }
            }
        }
        _ => {
            unreachable!("Wrong subcommand");
        }
    }
}