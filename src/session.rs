use channel;
use connection::Connection;
use protocol::{self, MethodFrame};
use table;
use table::TableEntry::{FieldTable, Bool, LongString};
use framing::Frame;
use amqp_error::{AMQPResult, AMQPError};

use std::sync::{Arc, Mutex};
use std::default::Default;
use std::collections::HashMap;
use std::sync::mpsc::{SyncSender, sync_channel};
use std::thread;
use std::cmp;

use url::{UrlParser, SchemeType, percent_encoding};

const CHANNEL_BUFFER_SIZE :usize = 100;


#[derive(Debug)]
pub struct Options <'a>  {
    pub host: &'a str,
    pub port: u16,
    pub login: &'a str,
    pub password: &'a str,
    pub vhost: &'a str,
    pub frame_max_limit: u32,
    pub channel_max_limit: u16,
    pub locale: &'a str
}

impl <'a>  Default for Options <'a>  {
    fn default() -> Options <'a>  {
        Options {
            host: "127.0.0.1", port: 5672, vhost: "",
            login: "guest", password: "guest",
            frame_max_limit: 131072, channel_max_limit: 65535,
            locale: "en_US"
        }
    }
}

pub struct Session {
    connection: Connection,
    channels: Arc<Mutex<HashMap<u16, SyncSender<Frame>> >>,
    channel_max_limit: u16,
    channel_zero: channel::Channel
}

impl Session {
    /// Use `open_url` to create new amqp session from a "amqp url"
    ///
    /// # Arguments
    /// * `url_string`: The format is: `amqp://username:password@host:port/virtual_host`
    ///
    /// Most of the params have their default, so you can just pass this:
    /// `"amqp://localhost/"` and it will connect to rabbitmq server, running on `localhost` on port `65535`,
    /// with login `"guest"`, password: `"guest"` to vhost `"/"`
    pub fn open_url(url_string: &str) -> AMQPResult<Session> {
        fn decode(string: &str) -> String {
            let input = string.as_bytes();
            percent_encoding::lossy_utf8_percent_decode(input)
        }
        fn clean_vhost(string: String) -> String {
            if &string.chars().next() == &Some('/') {
              String::from(decode(&string[1..]))
            } else {
              String::from(decode(&string[..]))
            }
        }

        let default: Options = Default::default();
        let mut url_parser = UrlParser::new();
        url_parser.scheme_type_mapper(scheme_type_mapper);
        let url = try!(url_parser.parse(url_string));
        let vhost = url.serialize_path().map(|vh| clean_vhost(vh)).unwrap_or(String::from(default.vhost.to_string()));
        let host  = url.domain().unwrap_or(default.host);
        let port = url.port().unwrap_or(default.port);
        let login = url.username().and_then(|u| match u { "" => None, _ => Some(decode(u))} ).unwrap_or(String::from(default.login));
        let password = url.password().map(|p| decode(p)).unwrap_or(String::from(default.password));
        let opts = Options { host: host, port: port,
         login: &login, password: &password,
         vhost: &vhost, ..Default::default()};
        Session::new(opts)
    }

    /// Initialize new rabbitmq session.
    /// You can use default options:
    /// # Example
    /// ```no_run
    /// use std::default::Default;
    /// use amqp::session::{Options, Session};
    /// let session = match Session::new(Options { .. Default::default() }){
    ///     Ok(session) => session,
    ///     Err(error) => panic!("Failed openning an amqp session: {:?}", error)
    /// };
    /// ```
    pub fn new(options: Options) -> AMQPResult<Session> {
        let connection = try!(Connection::open(options.host, options.port));
        let (channel_sender, channel_receiver) = sync_channel(CHANNEL_BUFFER_SIZE); //channel0
        let channels = Arc::new(Mutex::new(HashMap::new()));
        let channel_zero = channel::Channel::new(0, channel_receiver, connection.clone());
        try!(channels.lock().map_err(|_| AMQPError::SyncError)).insert(0, channel_sender);
        let con1 = connection.clone();
        let channels_clone = channels.clone();
        thread::spawn( || Session::reading_loop(con1, channels_clone ) );
        let mut session = Session {
            connection: connection,
            channels: channels,
            channel_max_limit: 65535,
            channel_zero: channel_zero
        };
        try!(session.init(options));
        Ok(session)
    }

    fn init(&mut self, options: Options) -> AMQPResult<()> {
        debug!("Starting init session");
        let frame = self.channel_zero.read(); //Start
        let method_frame = MethodFrame::decode(frame);
        let start : protocol::connection::Start = match method_frame.method_name(){
            "connection.start" => try!(protocol::Method::decode(method_frame)),
            meth => panic!("Unexpected method frame: {:?}", meth)
        };
        debug!("Received connection.start: {:?}", start);
        // * The client selects a security mechanism (Start-Ok).
        // * The server starts the authentication process, which uses the SASL challenge-response model. It sends
        // the client a challenge (Secure).
        // * The client sends an authentication response (Secure-Ok). For example using the "plain" mechanism,
        // the response consist of a login name and password.
        // * The server repeats the challenge (Secure) or moves to negotiation, sending a set of parameters such as

        let mut client_properties = table::new();
        let mut capabilities = table::new();
        capabilities.insert("publisher_confirms".to_string(), Bool(true));
        capabilities.insert("consumer_cancel_notify".to_string(), Bool(true));
        capabilities.insert("exchange_exchange_bindings".to_string(), Bool(true));
        capabilities.insert("basic.nack".to_string(), Bool(true));
        capabilities.insert("connection.blocked".to_string(), Bool(true));
        capabilities.insert("authentication_failure_close".to_string(), Bool(true));
        client_properties.insert("capabilities".to_string(), FieldTable(capabilities));
        client_properties.insert("product".to_string(), LongString("rust-amqp".to_string()));
        client_properties.insert("platform".to_string(), LongString("rust".to_string()));
        client_properties.insert("version".to_string(), LongString("0.1.8".to_string()));
        client_properties.insert("information".to_string(), LongString("https://github.com/Antti/rust-amqp".to_string()));

        debug!("Sending connection.start-ok");
        let start_ok = protocol::connection::StartOk {
            client_properties: client_properties, mechanism: "PLAIN".to_string(),
            response: format!("\0{}\0{}", options.login, options.password), locale: options.locale.to_string()};
        let response = self.channel_zero.raw_rpc(&start_ok);
        let tune : protocol::connection::Tune = match response.method_name() {
            "connection.tune" => try!(protocol::Method::decode(response)),
            "connection.close" => {
                let close_frame : protocol::connection::Close = try!(protocol::Method::decode(response));
                return Err(AMQPError::Protocol(format!("Connection was closed: {:?}", close_frame)));
            },
            response_method => return Err(AMQPError::Protocol(format!("Unexpected response: {}", response_method)))
        };
        debug!("Received tune request: {:?}", tune);

        self.channel_max_limit =  negotiate(tune.channel_max, self.channel_max_limit);
        self.connection.frame_max_limit = negotiate(tune.frame_max, options.frame_max_limit);
        self.channel_zero.connection.frame_max_limit = self.connection.frame_max_limit;
        let frame_max_limit = self.connection.frame_max_limit;
        let tune_ok = protocol::connection::TuneOk {
            channel_max: self.channel_max_limit,
            frame_max: frame_max_limit, heartbeat: 0};
        debug!("Sending connection.tune-ok: {:?}", tune_ok);
        self.channel_zero.send_method_frame(&tune_ok);

        debug!("Sending connection.open");
        let open = protocol::connection::Open{virtual_host: options.vhost.to_string(), capabilities: "".to_string(), insist: false };
        let _ : protocol::connection::OpenOk = try!(self.channel_zero.rpc(&open, "connection.open-ok"));
        debug!("Connection initialized. conneciton.open-ok recieved");
        info!("Session initialized");
        Ok(())
    }

    /// `open_channel` will open a new amqp channel:
    /// # Arguments
    ///
    /// * `channel_id` - channel number
    ///
    /// # Exmaple
    /// ```no_run
    /// use std::default::Default;
    /// use amqp::session::{Options, Session};
    /// let mut session =  Session::new(Options { .. Default::default() }).ok().unwrap();
    /// let channel = match session.open_channel(1){
    ///     Ok(channel) => channel,
    ///     Err(error) => panic!("Failed openning channel: {:?}", error)
    /// };
    /// ```
    pub fn open_channel(&mut self, channel_id: u16) -> AMQPResult<channel::Channel> {
        debug!("Openning channel: {}", channel_id);
        let (sender, receiver) = sync_channel(CHANNEL_BUFFER_SIZE);
        let mut channel = channel::Channel::new(channel_id, receiver, self.connection.clone());
        try!(self.channels.lock().map_err(|_| AMQPError::SyncError)).insert(channel_id, sender);
        try!(channel.open());
        Ok(channel)
    }

    pub fn close(&mut self, reply_code: u16, reply_text: String) {
        debug!("Closing session: reply_code: {}, reply_text: {}", reply_code, reply_text);
        let close = protocol::connection::Close {reply_code: reply_code, reply_text: reply_text, class_id: 0, method_id: 0};
        let _ : protocol::connection::CloseOk = self.channel_zero.rpc(&close, "connection.close-ok").ok().unwrap();
    }

    pub fn reading_loop(mut connection: Connection, channels: Arc<Mutex<HashMap<u16, SyncSender<Frame>>>>) -> () {
        debug!("Starting reading loop");
        loop {
            let frame = match connection.read() {
                Ok(frame) => frame,
                Err(some_err) => {error!("Error in reading loop: {:?}", some_err); break} //Notify session somehow. It should stop now.
            };
            let chans = channels.lock().unwrap();
            let ref target_channel = (*chans)[&frame.channel];
            target_channel.send(frame).ok().expect("Error sending packet");
            // match frame.frame_type {
            //     framing::METHOD => {},
            //     framing::HEADERS => {},
            //     framing::BODY => {},
            //     framing::HEARTBEAT => {}
            // }
            // Handle heartbeats
        }
        debug!("Exiting reading loop");
    }

}

fn negotiate<T : cmp::Ord>(their_value: T, our_value: T) -> T {
    cmp::min(their_value, our_value)
}
fn scheme_type_mapper(scheme: &str) -> SchemeType {
    match scheme{
        "amqp" => SchemeType::Relative(5672),
        _ => {panic!("Uknown scheme: {}", scheme)}
    }
}
