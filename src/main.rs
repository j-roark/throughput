mod throughput;

fn main() {
	let opts = throughput::Args::init();
	let mut conn = throughput::Session::new(opts);
	conn.try_start_session().unwrap();
	println!("Throughput: {:?}B/s", conn.profile.get_gross_throughput());
}
