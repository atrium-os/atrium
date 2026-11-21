//! A single request for the document currently attached to a sign-in sheet.
use sigillum_webauthn::{CreationOptions, DomError, Origin, RequestOptions, Sigillumd};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

enum Ceremony {
    Get(RequestOptions),
    Create(CreationOptions),
}

pub struct Pending {
    pub registering: bool,
    pub id: u32,
    socket: UnixStream,
    answer: Receiver<Result<String, DomError>>,
}

impl Pending {
    pub fn start(id: u32, origin: &Origin, json: &str, registering: bool) -> Result<Self, DomError> {
        let options = if registering {
            let options = CreationOptions::from_json(json)?;
            origin.rp_id(options.rp_id.as_deref())?;
            Ceremony::Create(options)
        } else {
            let options = RequestOptions::from_json(json)?;
            origin.rp_id(options.rp_id.as_deref())?;
            Ceremony::Get(options)
        };
        let socket = UnixStream::connect(sigillum_proto::client::SOCKET)
            .map_err(|_| DomError::NotAllowed)?;
        Self::on_socket(id, origin.clone(), options, socket).map_err(|_| DomError::NotAllowed)
    }

    fn on_socket(
        id: u32,
        origin: Origin,
        options: Ceremony,
        socket: UnixStream,
    ) -> std::io::Result<Self> {
        socket.set_write_timeout(Some(Duration::from_secs(2)))?;
        socket.set_read_timeout(Some(Duration::from_secs(125)))?;
        let mut client = Sigillumd::from_stream(socket.try_clone()?);
        let (sender, answer) = mpsc::channel();
        let registering = matches!(options, Ceremony::Create(_));
        std::thread::Builder::new()
            .name("credential-ceremony".into())
            .spawn(move || {
                let result = match options {
                    Ceremony::Get(options) => sigillum_webauthn::get(&mut client, &origin, None, &options).map(|a| a.to_json()),
                    Ceremony::Create(options) => sigillum_webauthn::create(&mut client, &origin, None, &options).map(|r| r.to_json()),
                };
                let _ = sender.send(result);
            })?;
        Ok(Self { id, registering, socket, answer })
    }

    pub fn poll(&self) -> Option<Result<String, DomError>> {
        match self.answer.try_recv() {
            Ok(value) => Some(value),
            Err(mpsc::TryRecvError::Empty) => None,
            Err(mpsc::TryRecvError::Disconnected) => Some(Err(DomError::NotAllowed)),
        }
    }
}

impl Drop for Pending {
    fn drop(&mut self) {
        // Sigillum retires this connection’s ceremony and sheet on EOF.
        // shutdown also covers a close before the worker thread wrote Assert.
        let _ = self.socket.shutdown(Shutdown::Both);
    }
}
