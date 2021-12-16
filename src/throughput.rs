use std::io::{stdin,stdout,Write};
use std::net::UdpSocket;
use std::time::{SystemTime, Duration};
use std::fmt;
use bytes::{BytesMut, BufMut};
use std::convert::TryInto;
use rand::{thread_rng, Rng};
use rand::distributions::Alphanumeric;
use std::env;

const BUFFER_SIZE: usize = 28;
// payload structure: 
/*
	--------UDP PACKET---------
	|  | SYN (4) | PAYLOAD |  |
	---------------------------
	syn = auto-increment current frame
	payload = random bytes with variable length

*/
// start sync structure:
/*
	
	host ->
	--------------UDP PACKET--------------
   |  | SYN (4) | SIZE (8) | DATA (16) |  |
	--------------------------------------
	HOST SYN CODES
	1: START | SIZE | TIME(s)
	2: ERR
	3: STOP

*/
// client ack structure:
/*
	
	client ->
	(stop and wait arq)
	--------------UDP PACKET--------------
   |  | ACK (4) | SIZE (8) | DATA (16) |  |
	--------------------------------------
	CLIENT ACK CODES
	1: OK | SIZE | MICROS FROM LAST
	2: ERR
	3: STOP
	4: SYN | SIZE |
	5: RESEND FROM SYN | NULL | LAST SYN

*/

enum Signals { Start, Ok, Syn, Err, Stop, ResendFromSyn(u128) }
pub enum PacketType { Ok, Dropped, Jittered }
struct Packet { sig: [u8; 4], size: [u8; 8], data: [u8; 16] } // in
struct Signal { sig: i32, size: Option<usize>, data: Option<u128> } // out

impl Signal {
	fn export(&self) -> BytesMut {
		let mut out_buf = BytesMut::with_capacity(BUFFER_SIZE);
		out_buf.put_i32(self.sig);
		match self.size {
			Some(s) => out_buf.put_slice(&s.to_be_bytes()),
			None => {}
		}
		match self.data {
			Some(d) => out_buf.put_u128(d),
			None => {}
		} out_buf
	}

	fn new(msg: Signals, session: &Session, size: Option<usize>, window: Option<u128>) -> Signal {
		match msg {
			Signals::Start => Signal {
				sig: 1,
				size: Some(session.options.size), 
				data: Some(session.options.time)
			},
			Signals::Ok => Signal {
				sig: 1, 
				size: size, 
				data: window
			},
			Signals::Syn => Signal {
				sig: 1,
				size: Some(session.options.size), 
				data: None
			},
			Signals::Err => Signal {sig: 2, size: None, data: None},
			Signals::Stop => Signal {sig: 3, size: None, data: None},
			Signals::ResendFromSyn(syn) => Signal { 
				sig: 5,
				size: None,
				data: Some(syn)
			}
		}
	}
}

impl Packet {
	fn build_from(buf: [u8; BUFFER_SIZE]) -> Packet {
		// size is known so try into won't fail
		Packet {
			sig: { buf[0..4].try_into().unwrap() },
			size: { buf[4..12].try_into().unwrap() },
			data: { buf[12..28].try_into().unwrap() }
		}
	}

	fn get_sig(&self) -> i32 {
		i32::from_be_bytes(self.sig)
	}

	fn get_size(&self) -> usize {
		usize::from_be_bytes(self.size)
	}

	fn get_data(&self) -> u128 {
		u128::from_be_bytes(self.data)
	}

	fn get_time(&self) -> Duration  {
		let time = u128::from_be_bytes(self.data) as u64;
		Duration::from_micros(time)
	}

}

type Instant = (SystemTime, Duration, usize, PacketType);
pub struct Profile { pub times: Vec<Instant> }
pub enum Source { Remote, Host }

#[derive(Debug)]
pub enum ThroughputError {
	HostStopSignal,
	ClientErrorSignal,
	ClientStopSignal,
	InvalidSignal,
	UDPError,
	MaximumFailedRequsts
}

impl fmt::Display for ThroughputError {
	fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
		match *self {
			ThroughputError::HostStopSignal=> f.write_str("Host sent a stop signal."),
			ThroughputError::ClientErrorSignal=> f.write_str("An unkown clientside error has occured."),
			ThroughputError::ClientStopSignal=> f.write_str("Client sent a stop signal."),
			ThroughputError::InvalidSignal=> f.write_str("Invalid signal recieved."),
			ThroughputError::UDPError => f.write_str("An error with UDP has occured."),
			ThroughputError::MaximumFailedRequsts => f.write_str("Too many initialization attempts.")
		}
	}
}

const MAXIMUM_SYN_REQUESTS: i16 = 100;

pub struct Session {
	options: Args,
	pub profile: Profile,
	socket: UdpSocket,
	initialized: bool,
	init_time: SystemTime,
	dead_time: Option<SystemTime>,
	syn_payload: Vec<u8>,
	syn: u128
}

impl Session {

	// beware shitty netcode ahead ;(
	pub fn new(opt: Args) -> Session {
		let socket = UdpSocket::bind(
			opt.get_addr(Source::Host)
		).expect("Inavalid IP or Port");
		socket.connect(
			opt.get_addr(Source::Remote)
		).expect("Unable to connect to remote host");
		return Session {
			options: opt,
			profile: Profile::new(),
			socket: socket,
			initialized: false,
			init_time: SystemTime::now(),
			dead_time: None,
			syn_payload: Vec::new(),
			syn: 1u128
		};
	}

	fn slice_payload(&self) -> &[u8] { let res: &[u8] = &self.syn_payload; res }

	fn build_payload(&mut self) { // payload -> [[syn], [payload]]
		let mut b_syn: Vec<u8> = self.syn.to_be_bytes().to_vec().clone();
		let mut b_pay: Vec<u8> = self.options.payload.clone();
		b_syn.append(&mut b_pay);
		self.syn_payload = b_syn;
		self.syn = self.syn + 1u128;
	}

	fn update_syn(&mut self, packet: Vec<u8>) -> Result<(), u128> {
		assert!(packet.len() >= 16);
		let data: [u8; 16] = packet[0..16].try_into().unwrap();
		let syn = u128::from_be_bytes(data);
		match syn == self.syn {
			true => { self.syn = self.syn + 1; Ok(()) },
			false => Err(syn)
		}
	}

	fn init_host(&mut self) -> Result<usize, ThroughputError> {
		let mut attempts = 0;
		let signals = Signal::new(Signals::Start, &self, None, None);
		println!("Running host initialization...");
		return loop {
			self.socket.send(&signals.export()).expect("Unable to initialize socket");
			let mut buf = [0u8; BUFFER_SIZE];
			match self.socket.recv(&mut buf) {
				Ok(_) => {},
				Err(_) => { print!("e"); break Err(ThroughputError::UDPError) }
			}
			
			let packet = Packet::build_from(buf);
			let sig = packet.get_sig()
			if sig == 1 {
				let size = packet.get_size();
				if packet.get_size() == 0 {
					attempts = attempts + 1;
					if attempts >= MAXIMUM_SYN_REQUESTS {
						break Err(ThroughputError::MaximumFailedRequsts)
					}
					continue;
				}
				if size == self.options.size {
					self.initialized = true;
					println!("Successfully initialized!");
					break Ok(size)
				} else {
					attempts = attempts + 1;
					if attempts >= MAXIMUM_SYN_REQUESTS {
						break Err(ThroughputError::MaximumFailedRequsts)
					}
				}
			} else if sig == 3 {
				break Err(ThroughputError::ClientStopSignal)
			}
		}
	}

	fn reply_err(&self) {
		let r_err = Signal::new(Signals::Err, &self, None, None);
		self.socket
			.send(&r_err.export())
			.expect("Unable to initialize socket");
	}

	fn init_client(&mut self) -> Result<usize, ThroughputError> {
		let mut buf = [0u8; BUFFER_SIZE];
		println!("Running client initialization...");
		return loop {
			match self.socket.recv(&mut buf) {
				Ok(_) => {},
				Err(_) => { println!("e"); return Err(ThroughputError::UDPError); }
			}
			let packet = Packet::build_from(buf);
			match packet.get_sig() {
				1 => {
					let size = packet.get_size();
					let time = packet.get_data();
					match size {
						0 => { self.reply_err(); continue; }
						_ => self.options.size = size
					}
					match time {
						0 => { self.reply_err(); continue; },
						_ => self.options.time = time
					}
					let reply = Signal::new(
						Signals::Syn,
						&self,
						None,
						None
					);
					self.socket
						.send(&reply.export())
						.expect("Unable to initialize socket");
					println!("Successfully initialized!");
					break Ok(self.options.size);
				},
				2 => {}
				3 => break Err(ThroughputError::HostStopSignal),
				_ => { self.reply_err(); continue; }
			}
		}
	}

	fn run_host(&mut self) -> Result<usize, ThroughputError> {
		let start = SystemTime::now();
		self.build_payload();
		self.socket.send(self.slice_payload()).expect("couldn't send to client");
		let mut buf = [0u8; BUFFER_SIZE];
		loop {
			match self.socket.recv(&mut buf) {
				Ok(_) => {},
				Err(e) => { print!("{:?}", e); break Err(ThroughputError::UDPError) }
			}
			
			let packet = &Packet::build_from(buf);
			match packet.get_sig() {
				1 => {
					let size: usize = packet.get_size();
					match size > self.options.size {
						false => break Err(ThroughputError::InvalidSignal),
						true => {
							let window = packet.get_time();
							self.profile.times.push(
								(start, window, size, PacketType::Ok)
							);
							break Ok(size);
						},
					}
				}
				2 => { break Err(ThroughputError::ClientErrorSignal) },
				3 => { break Err(ThroughputError::ClientStopSignal) },
				4 | 5 => { // resynchronize request
					self.syn = packet.get_data(); break Ok(0)
				},
				_ => { break Err(ThroughputError::InvalidSignal) }
			}
		}
	}

	fn run_client(&mut self) -> Result<usize, ThroughputError> {
		let start = SystemTime::now(); 
		let mut in_buf = vec![0u8; self.options.size + 16];
		loop {
			if start.elapsed().unwrap().as_secs() < self.options.time as u64{
				match self.socket.recv(&mut in_buf) {
					Ok(size) => {
						if size == 0 {
							self.reply_err(); return Err(ThroughputError::InvalidSignal)
						}
						match self.update_syn(in_buf) {
							Ok(_) => {},
							Err(syn) => {
								let d = start.elapsed().unwrap();
								let reply = Signal::new(
									Signals::ResendFromSyn(syn),
									&self,
									None,
									None
								);
								self.socket.send(&reply.export()).expect("couldn't reply to host");
								self.profile.times.push(
									(start, d, size, PacketType::Jittered)
								);
								return Ok(size)
							},
						}
						let d = start.elapsed().unwrap();
						let reply = Signal::new(
							Signals::Ok,
							&self, 
							Some(size),
							Some(d.as_micros())
						);
						self.socket.send(&reply.export()).expect("couldn't reply to host");
						self.profile.times.push(
							(start, d, size, PacketType::Ok)
						);
						return Ok(size)
					},
					Err(e) => { println!("{:?}", e); return Err(ThroughputError::UDPError) }
				}
			}
		}
	}

	pub fn try_start_session(&mut self) -> Result<(), ThroughputError>{
		match self.options.is_client {
			true => {
				match self.init_client() {
					Ok(_) => {},
					Err(e) => panic!("{:?}", e)
				}
				
				let now = SystemTime::now();
				loop { match now.elapsed() {
					Ok(d) => {
						if d.as_secs() < self.options.time as u64 {
							match self.run_client() {
								Ok(_) => {
									self.dead_time = Some(SystemTime::now());
								},
								Err(e) => panic!("{}", e)
							}
						} else { break Ok(()) }
					},
					Err(e) => {
						panic!("{:?}", e);
					}
				}}
			},
			},
			false => {
				match self.init_host() {
					Ok(_) => {},
					Err(e) => panic!("{:?}", e)
				}
				let now = SystemTime::now();
				loop { match now.elapsed() {
					Ok(d) => {
						if d.as_secs() >= self.options.time as u64 {
							break Ok(())
						}
						match self.run_host() {
							Ok(_) => { self.dead_time = Some(SystemTime::now()); },
							Err(e) => panic!("{}", e)
						}
					},
					Err(e) => {
						panic!("{:?}", e);
					}
				}}
			}
		}
	}

}

impl std::ops::Drop for Session {
	fn drop(&mut self) {
		// once the parent session {} object is dropped this will tell the other side to stop
		// this is to prevent one side from hanging up if the other side quits (ctrl + c or anything)
		let sig = Signal::new(Signals::Stop, &self, None, None);
		self.socket.send(&sig.export()).expect("Unable to send drop message");
	}
}


impl fmt::Display for Session {
	fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
		f.write_str(
			format!(
				"Throughput UDP session from: {} to: {}",
				self.options.get_addr(Source::Host),
				self.options.get_addr(Source::Remote)
			).as_str()
		)
	}
}

fn prompt(question: &str) -> String {
	let mut input = String::new();
	print!("{}", question);
	let _ = stdout().flush();
	stdin()
		.read_line(&mut input)
		.expect("Did not enter a correct string");
	return input.trim().to_lowercase();
}

fn try_get_default_addr() -> Option<String> {
	// special thanks https://github.com/egmkang/local_ipaddress/
	let socket = match UdpSocket::bind("0.0.0.0:0") {
		Ok(s) => s,
		Err(_) => return None,
	};

	match socket.connect("8.8.8.8:80") {
		Ok(()) => (),
		Err(_) => return None,
	};

	match socket.local_addr() {
		Ok(addr) => return Some(addr.ip().to_string()),
		Err(_) => return None,
	};
}

pub struct Args {
	destination_ip: String,
	destination_port: i32,
	source_ip: String,
	source_port: i32,
	pub time: u128,
	pub size: usize,
	payload: Vec<u8>,
	pub is_client: bool
}

impl Args {
	pub fn init() -> Args {
		// cmd options
		let args: Vec<String> = env::args().collect();
		let mut destination_ip = String::new();
		let mut port: i32 = 55667;
		let mut speed = 0i32;
		let mut size: usize = 1024;
		let mut is_client = false;
		let mut counter: usize = 0;
		let mut time: u128 = 10;
		for arg in &args {
			counter = counter + 1;
			match arg.trim().to_lowercase().as_ref() {
				"-d" | "--d" => {
					destination_ip = args[counter].trim().to_lowercase(); continue;
				},
				"-p" | "--p" => { port = {
					match args[counter].trim().to_lowercase().as_str().parse::<i32>() {
						Ok(int) => int,
						Err(_) => {
							println!("Invalid input for port!");
							loop {
								let s = prompt("UDP Port: ");
								match s.as_str().parse::<i32>() {
									Ok(i) => {
										if i < 65353 && i > 1023 { break i
										} else { println!("Range invalid!"); continue; }
									}
									Err(_) => { println!("Input invalid!"); continue; }
								};
							}
						}
					}
				}; continue; },
				"-s" | "--s" => { speed = {
					match args[counter].trim().to_lowercase().as_str().parse::<i32>() {
						Ok(int) => int,
						Err(_) => {
							println!("Invalid input for speed!");
							loop {
								let s = prompt("Speed (1-10): ");
								match s.as_str().parse::<i32>() {
									Ok(i) => {
										if i >= 1 && i <= 10 { break i
										} else { println!("Input range invalid!"); continue; }
									},
									Err(_) => { println!("Input invalid!"); continue;  }
								};
							}
						}
					}
				}; continue; },
				"-z" | "--z" => { size = {
					match args[counter].trim().to_lowercase().as_str().parse::<i32>() {
						Ok(int) => int as usize,
						Err(_) => {
							println!("Invalid input for size!");
							loop {
								let s = prompt("Message size? (Bytes): ");
								match s.as_str().parse::<i32>() {
									Ok(i) => i as usize,
									Err(_) => { println!("Input invalid!"); continue;  }
								};
							}
						}
					}
				}; continue; },
				"-t" | "--t" => { time = {
					match args[counter].trim().to_lowercase().as_str().parse::<u128>() {
						Ok(int) => int,
						Err(_) => {
							println!("Invalid input for time!");
							loop {
								let s = prompt("Time? (s): ");
								match s.as_str().parse::<i32>() {
									Ok(i) => i as usize,
									Err(_) => { println!("Input invalid!"); continue;  }
								};
							}
						}
					}
				}; continue; },
				"-c" | "--c" => { is_client = true; continue; },
				_ => { continue; }
			}
		}

		let rand_string: String = thread_rng()
			.sample_iter(&Alphanumeric)
			.take(size)
			.map(char::from)
			.collect();
		if destination_ip.len() == 0 {
			destination_ip = prompt("Destination IP or Domain Name: ");
		}
		let source_ip =
			match try_get_default_addr() {
				Some(s) => s,
				None => {
					println!("Unable to find an address to bind to.");
					prompt("Source interface address: ")
				}
			};

		return Args {
			destination_ip: destination_ip,
			destination_port: port.clone(),
			source_ip: source_ip,
			source_port: port,
			time: time,
			size: size,
			payload: rand_string.as_bytes().to_vec(),
			is_client: is_client,
		}
	}

	pub fn get_addr(&self, src: Source) -> String {
		match src {
			Source::Host => format!("{}:{}", self.source_ip, self.source_port),
			Source::Remote =>
				format!("{}:{}", self.destination_ip, self.destination_port)
		}
	}

}

impl Profile {

	pub fn new() -> Profile { Profile { times: Vec::new() } }
	// TODO add more diagnostics and computation of data in this struct
	// perhaps an export to MatPlotLib or .xlsx?
	pub fn get_gross_throughput(&self) -> f64 {
		let mut size_total: usize = BUFFER_SIZE; // for the init ;)
		let mut time_total_ms: u128 = 0;
		let mut total_jitter: i32 = 0;
		for (_, duration, size, packet_type) in &self.times {
			match packet_type {
				PacketType::Ok => {
					size_total = size_total + size;
					time_total_ms = duration.as_micros() + time_total_ms;
				},
				_ => { total_jitter = total_jitter + 1 }
			}
		}
		match total_jitter == 0 {
			true => println!("Jitter: 0%"),
			false => println!("Jitter: {}%", total_jitter / self.times.len() as i32),
		}
		let duration = Duration::from_micros(time_total_ms as u64);
		println!("Total transmitted: {:?}B in {:?}s", size_total, duration.as_secs());
		size_total as f64 / duration.as_secs() as f64
	}

}
