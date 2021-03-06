extern crate amqp;
extern crate env_logger;

use amqp::session::Options;
use amqp::session::Session;
use amqp::protocol;
use amqp::table;
use amqp::basic::Basic;
use amqp::channel::Channel;
use std::default::Default;

fn consumer_function(channel: &mut Channel, deliver: protocol::basic::Deliver, headers: protocol::basic::BasicProperties, body: Vec<u8>){
    println!("Got a delivery:");
    println!("Deliver info: {:?}", deliver);
    println!("Content headers: {:?}", headers);
    println!("Content body: {:?}", body);
    println!("Content body(as string): {:?}", String::from_utf8(body));
    channel.basic_ack(deliver.delivery_tag, false);
}

fn main() {
    env_logger::init().unwrap();
    let mut session = Session::new(Options{.. Default::default()}).ok().expect("Can't create session");
    let mut channel = session.open_channel(1).ok().expect("Error openning channel 1");
    println!("Openned channel: {:?}", channel.id);

    let queue_name = "test_queue";
    //queue: &str, passive: bool, durable: bool, exclusive: bool, auto_delete: bool, nowait: bool, arguments: Table
    let queue_declare = channel.queue_declare(queue_name, false, true, false, false, false, table::new());

    println!("Queue declare: {:?}", queue_declare);
    channel.basic_prefetch(10);
    //queue: &str, consumer_tag: &str, no_local: bool, no_ack: bool, exclusive: bool, nowait: bool, arguments: Table
    println!("Declaring consumer...");
    let consumer_name = channel.basic_consume(consumer_function, queue_name, "", false, false, false, false, table::new());

    println!("Starting consumer {:?}", consumer_name);
    channel.start_consuming();

    channel.close(200, "Bye".to_string());
    session.close(200, "Good Bye".to_string());
}
