use crate::alert::*;
use crate::buffer::CryptoBuffer;
use crate::connection::*;
use crate::handshake::ServerHandshake;
use crate::key_schedule::KeySchedule;
use crate::record::{ClientRecord, ServerRecord};
use crate::TlsError;
use embedded_io::Error as _;
use embedded_io::{
    asynch::{Read as AsyncRead, Write as AsyncWrite, DirectRead, DirectReadHandle},
    Io,
};
use rand_core::{CryptoRng, RngCore};

use crate::application_data::ApplicationData;
use heapless::spsc::Queue;

pub use crate::config::*;

// Some space needed by TLS record
const TLS_RECORD_OVERHEAD: usize = 128;

/// Type representing an async TLS connection. An instance of this type can
/// be used to establish a TLS connection, write and read encrypted data over this connection,
/// and closing to free up the underlying resources.
pub struct TlsConnection<'a, Socket, CipherSuite>
where
    Socket: AsyncRead + AsyncWrite + 'a,
    CipherSuite: TlsCipherSuite + 'static,
{
    delegate: Socket,
    key_schedule: KeySchedule<CipherSuite::Hash, CipherSuite::KeyLen, CipherSuite::IvLen>,
    record_buf: &'a mut [u8],
    opened: bool,
}

pub struct TlsReadHandle<'b> {
    buffer: CryptoBuffer<'b>,
    is_completed: bool,
}

impl<'a, Socket, CipherSuite> TlsConnection<'a, Socket, CipherSuite>
where
    Socket: AsyncRead + AsyncWrite + 'a,
    CipherSuite: TlsCipherSuite + 'static,
{
    /// Create a new TLS connection with the provided context and a async I/O implementation
    ///
    /// NOTE: The record buffer should be sized to fit an encrypted TLS record and the TLS handshake
    /// record. The maximum value of a TLS record is 16 kB, which should be a safe value to use.
    pub fn new(delegate: Socket, record_buf: &'a mut [u8]) -> Self {
        Self {
            delegate,
            opened: false,
            key_schedule: KeySchedule::new(),
            record_buf,
        }
    }

    /// Open a TLS connection, performing the handshake with the configuration provided when creating
    /// the connection instance.
    ///
    /// The handshake may support certificates up to CERT_SIZE.
    ///
    /// Returns an error if the handshake does not proceed. If an error occurs, the connection instance
    /// must be recreated.
    pub async fn open<'m, RNG: CryptoRng + RngCore + 'm, Verifier: TlsVerifier<CipherSuite> + 'm>(
        &mut self,
        context: TlsContext<'m, CipherSuite, RNG>,
    ) -> Result<(), TlsError>
    where
        'a: 'm,
    {
        let mut handshake: Handshake<CipherSuite, Verifier> =
            Handshake::new(Verifier::new(context.config.server_name));
        let mut state = State::ClientHello;

        loop {
            let next_state = state
                .process::<_, _, _, Verifier>(
                    &mut self.delegate,
                    &mut handshake,
                    self.record_buf,
                    &mut self.key_schedule,
                    context.config,
                    context.rng,
                )
                .await?;
            trace!("State {:?} -> {:?}", state, next_state);
            state = next_state;
            if let State::ApplicationData = state {
                self.opened = true;
                break;
            }
        }

        Ok(())
    }

    /// Encrypt and send the provided slice over the connection. The connection
    /// must be opened before writing.
    ///
    /// Returns the number of bytes written.
    pub async fn write(&mut self, buf: &[u8]) -> Result<usize, TlsError> {
        if self.opened {
            let mut wp = 0;
            let mut remaining = buf.len();

            let max_block_size = self.record_buf.len() - TLS_RECORD_OVERHEAD;
            while remaining > 0 {
                let delegate = &mut self.delegate;
                let key_schedule = &mut self.key_schedule;
                let to_write = core::cmp::min(remaining, max_block_size);
                let record: ClientRecord<'a, '_, CipherSuite> =
                    ClientRecord::ApplicationData(&buf[wp..to_write]);

                let (_, len) = encode_record(self.record_buf, key_schedule, &record)?;

                delegate
                    .write(&self.record_buf[..len])
                    .await
                    .map_err(|e| TlsError::Io(e.kind()))?;
                key_schedule.increment_write_counter();
                wp += to_write;
                remaining -= to_write;
            }

            Ok(buf.len())
        } else {
            Err(TlsError::MissingHandshake)
        }
    }

    /// Read and decrypt data filling the provided slice. The slice must be able to
    /// keep the expected amount of data that can be received in one record to avoid
    /// loosing data.
    pub async fn read<'b>(&'b mut self) -> Result<TlsReadHandle<'b>, TlsError> {
        if self.opened {
            let socket = &mut self.delegate;
            let key_schedule = &mut self.key_schedule;
            let record =
                decode_record::<Socket, CipherSuite>(socket, self.record_buf, key_schedule).await?;
            let mut records = Queue::new();
            decrypt_record::<CipherSuite>(key_schedule, &mut records, record)?;

            let mut handle = None;
            while let Some(record) = records.dequeue() {
                if let Some(buffer) = match record {
                    ServerRecord::ApplicationData(ApplicationData { header: _, data }) => {
                        Ok(Some(data))
                    }
                    ServerRecord::Alert(alert) => {
                        if let AlertDescription::CloseNotify = alert.description {
                            Err(TlsError::ConnectionClosed)
                        } else {
                            Err(TlsError::InternalError)
                        }
                    }
                    ServerRecord::ChangeCipherSpec(_) => Err(TlsError::InternalError),
                    ServerRecord::Handshake(ServerHandshake::NewSessionTicket(_)) => {
                        // Ignore
                        Ok(None)
                    }
                    _ => {
                        unimplemented!()
                    }
                }? {
                    handle = Some(TlsReadHandle {
                        buffer,
                        is_completed: false,
                    })
                }
            }
            Ok(handle.unwrap_or_else(|| TlsReadHandle {
                buffer: CryptoBuffer::empty(),
                is_completed: true,
            }))
        } else {
            Err(TlsError::MissingHandshake)
        }
    }

    /// Close a connection instance, returning the ownership of the config, random generator and the async I/O provider.
    async fn close_internal(&mut self) -> Result<(), TlsError> {
        let record = ClientRecord::Alert(
            Alert::new(AlertLevel::Warning, AlertDescription::CloseNotify),
            self.opened,
        );

        let mut key_schedule = &mut self.key_schedule;
        let delegate = &mut self.delegate;
        let record_buf = &mut self.record_buf;

        let (_, len) = encode_record::<CipherSuite>(record_buf, &mut key_schedule, &record)?;

        delegate
            .write(&record_buf[..len])
            .await
            .map_err(|e| TlsError::Io(e.kind()))?;

        key_schedule.increment_write_counter();

        Ok(())
    }

    /// Close a connection instance, returning the ownership of the async I/O provider.
    pub async fn close(mut self) -> Result<Socket, (Socket, TlsError)> {
        match self.close_internal().await {
            Ok(()) => Ok(self.delegate),
            Err(e) => Err((self.delegate, e)),
        }
    }
}

impl<'a, Socket, CipherSuite> Io for TlsConnection<'a, Socket, CipherSuite>
where
    Socket: AsyncRead + AsyncWrite + 'a,
    CipherSuite: TlsCipherSuite + 'static,
{
    type Error = TlsError;
}

impl<'m> DirectReadHandle<'m> for TlsReadHandle<'m> {
    fn as_slice(&self) -> &[u8] {
        self.buffer.as_slice()
    }

    fn is_completed(&self) -> bool {
        self.is_completed
    }
}

impl<'a, Socket, CipherSuite> DirectRead for TlsConnection<'a, Socket, CipherSuite>
where
    Socket: AsyncRead + AsyncWrite + 'a,
    CipherSuite: TlsCipherSuite + 'static,
{
    type Handle<'m> = TlsReadHandle<'m>;

    async fn read<'m>(&'m mut self) -> Result<Self::Handle<'m>, Self::Error> {
        TlsConnection::read(self).await
    }
}

impl<'a, Socket, CipherSuite> AsyncWrite for TlsConnection<'a, Socket, CipherSuite>
where
    Socket: AsyncRead + AsyncWrite + 'a,
    CipherSuite: TlsCipherSuite + 'static,
{
    async fn write(&mut self, buf: &[u8]) -> Result<usize, Self::Error> {
        TlsConnection::write(self, buf).await
    }

    async fn flush(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }
}
