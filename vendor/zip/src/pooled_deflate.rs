//! Reuse flate2 codec allocations between ZIP members on the same worker.
//! ZIP members remain independent streams: reset discards the prior dictionary.
use std::{
    cell::RefCell,
    io::{self, Write},
};

enum Engine {
    #[cfg(feature = "deflate-flate2")]
    Rs(flate2::Compress),
    Ng(flate2_zlib_ng::Compress),
}

impl Engine {
    fn reset(&mut self) {
        match self {
            #[cfg(feature = "deflate-flate2")]
            Self::Rs(codec) => codec.reset(),
            Self::Ng(codec) => codec.reset(),
        }
    }

    fn step(
        &mut self,
        input: &[u8],
        output: &mut [u8],
        flush: u8,
    ) -> io::Result<(usize, usize, bool)> {
        // Keep each flate2 type in its own match arm; Cargo cannot unify backends.
        macro_rules! step {
            ($codec:expr, $backend:ident) => {{
                let before_in = $codec.total_in();
                let before_out = $codec.total_out();
                let status = $codec
                    .compress(
                        input,
                        output,
                        match flush {
                            0 => $backend::FlushCompress::None,
                            1 => $backend::FlushCompress::Sync,
                            _ => $backend::FlushCompress::Finish,
                        },
                    )
                    .map_err(io::Error::other)?;
                Ok((
                    ($codec.total_in() - before_in) as usize,
                    ($codec.total_out() - before_out) as usize,
                    status == $backend::Status::StreamEnd,
                ))
            }};
        }
        match self {
            #[cfg(feature = "deflate-flate2")]
            Self::Rs(codec) => step!(codec, flate2),
            Self::Ng(codec) => step!(codec, flate2_zlib_ng),
        }
    }
}

struct State {
    level: u32,
    engine: Engine,
    output: Vec<u8>,
}

thread_local! {
    // At most one context per backend per worker, never shared across threads.
    static CACHE: RefCell<[Option<State>; 2]> = const { RefCell::new([None, None]) };
}

pub(super) struct PooledDeflater<W: Write> {
    inner: Option<W>,
    state: Option<State>,
    slot: usize,
}

impl<W: Write> PooledDeflater<W> {
    pub(super) fn new(inner: W, level: u32, ng: bool) -> Self {
        let slot = usize::from(ng);
        let mut state = CACHE
            .with(|cache| cache.borrow_mut()[slot].take())
            .filter(|state| state.level == level)
            .unwrap_or_else(|| {
                let engine = if ng {
                    Engine::Ng(flate2_zlib_ng::Compress::new(
                        flate2_zlib_ng::Compression::new(level),
                        false,
                    ))
                } else {
                    #[cfg(feature = "deflate-flate2")]
                    {
                        Engine::Rs(flate2::Compress::new(
                            flate2::Compression::new(level),
                            false,
                        ))
                    }
                    #[cfg(not(feature = "deflate-flate2"))]
                    {
                        unreachable!("the Rust deflater is disabled")
                    }
                };
                State {
                    level,
                    engine,
                    output: vec![0; 64 * 1024],
                }
            });
        state.engine.reset();
        Self {
            inner: Some(inner),
            state: Some(state),
            slot,
        }
    }

    pub(super) fn get_ref(&self) -> &W {
        self.inner.as_ref().expect("live writer")
    }
    pub(super) fn get_mut(&mut self) -> &mut W {
        self.inner.as_mut().expect("live writer")
    }

    pub(super) fn finish(mut self) -> io::Result<W> {
        let state = self.state.as_mut().expect("live codec");
        loop {
            let (_, written, ended) = state.engine.step(&[], &mut state.output, 2)?;
            self.inner
                .as_mut()
                .expect("live writer")
                .write_all(&state.output[..written])?;
            if ended {
                break;
            }
            if written == 0 {
                return Err(io::Error::other("DEFLATE finalization made no progress"));
            }
        }
        Ok(self.inner.take().expect("live writer"))
    }
}

impl<W: Write> Write for PooledDeflater<W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let state = self.state.as_mut().expect("live codec");
        let mut position = 0;
        while position < bytes.len() {
            let (consumed, written, _) =
                state
                    .engine
                    .step(&bytes[position..], &mut state.output, 0)?;
            self.inner
                .as_mut()
                .expect("live writer")
                .write_all(&state.output[..written])?;
            position += consumed;
            if consumed == 0 && written == 0 {
                return Err(io::Error::other("DEFLATE made no progress"));
            }
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        let state = self.state.as_mut().expect("live codec");
        loop {
            let (_, written, _) = state.engine.step(&[], &mut state.output, 1)?;
            self.inner
                .as_mut()
                .expect("live writer")
                .write_all(&state.output[..written])?;
            if written < state.output.len() {
                break;
            }
        }
        self.get_mut().flush()
    }
}

impl<W: Write> Drop for PooledDeflater<W> {
    fn drop(&mut self) {
        let _ = CACHE.try_with(|cache| cache.borrow_mut()[self.slot] = self.state.take());
    }
}
