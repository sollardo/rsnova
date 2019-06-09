use bytes::{BufMut, BytesMut};
use tokio::prelude::*;
use tokio_io::io::{read_exact, write_all};

use orion::hazardous::aead::{chacha20poly1305, xchacha20poly1305};
use std::io::{Error, ErrorKind};

use crate::mux::event::*;

pub const METHOD_CHACHA20_POLY1305: &str = "chacha20poly1305";
pub const METHOD_NONE: &str = "none";

pub struct CryptoContext {
    pub key: String,
    pub encrypt_nonce: u64,
    pub decrypt_nonce: u64,
    pub encrypter: EncryptFunc,
    pub decrypter: DecryptFunc,
}

type DecryptError = (u32, &'static str);

type EncryptFunc = fn(ctx: &CryptoContext, ev: &Event, out: &mut BytesMut);
type DecryptFunc = fn(ctx: &CryptoContext, buf: &mut BytesMut) -> Result<Event, DecryptError>;

impl CryptoContext {
    pub fn encrypt(&mut self, ev: &Event, out: &mut BytesMut) {
        (self.encrypter)(&self, ev, out);
        self.encrypt_nonce = self.encrypt_nonce + 1;
    }
    pub fn decrypt(&mut self, buf: &mut BytesMut) -> Result<Event, DecryptError> {
        let r = (self.decrypter)(&self, buf);
        match r {
            Ok(_) => {
                self.decrypt_nonce = self.decrypt_nonce + 1;
            }
            _ => {}
        }
        r
    }

    pub fn reset(&mut self, nonce: u64) {
        self.decrypt_nonce = nonce;
        self.encrypt_nonce = nonce;
    }
}

pub fn read_encrypt_event<T: AsyncRead>(
    mut ctx: CryptoContext,
    r: T,
) -> impl Future<Item = (CryptoContext, T, Event), Error = std::io::Error> {
    let buf = vec![0; EVENT_HEADER_LEN];
    read_exact(r, buf).and_then(move |(_stream, data)| {
        let mut buf = BytesMut::from(data);
        let r = ctx.decrypt(&mut buf);
        match r {
            Ok(ev) => future::Either::A(future::ok((ctx, _stream, ev))),
            Err((n, reason)) => {
                if reason.len() > 0 {
                    return future::Either::A(future::err(Error::from(
                        ErrorKind::PermissionDenied,
                    )));
                }
                let data_buf = vec![0; n as usize];
                let r = read_exact(_stream, data_buf).and_then(move |(_r, _body)| {
                    buf.reserve(n as usize);
                    buf.put_slice(&_body[..]);
                    // let ev = ctx.decrypt(&mut buf).unwrap();
                    // Ok((ctx, _r, ev))
                    match ctx.decrypt(&mut buf) {
                        Ok(ev) => return Ok((ctx, _r, ev)),
                        Err(e) => return Err(Error::from(ErrorKind::InvalidInput)),
                    }
                });
                future::Either::B(r)
            }
        }
    })
}

pub fn none_encrypt_event(ctx: &CryptoContext, ev: &Event, out: &mut BytesMut) {
    out.put_u32_le(ev.header.flag_len);
    out.put_u32_le(ev.header.stream_id);

    if ev.body.len() > 0 {
        out.put_slice(&ev.body[..]);
    }
}

pub fn none_decrypt_event(ctx: &CryptoContext, buf: &mut BytesMut) -> Result<Event, DecryptError> {
    if buf.len() < EVENT_HEADER_LEN {
        //println!("decrypt error0:{}", buf.len());
        return Err((EVENT_HEADER_LEN as u32 - buf.len() as u32, ""));
    }
    let mut xbuf: [u8; 4] = Default::default();
    xbuf.copy_from_slice(&buf[0..4]);
    let e1 = u32::from_le_bytes(xbuf);
    xbuf.copy_from_slice(&buf[4..8]);
    let e2 = u32::from_le_bytes(xbuf);

    let header = Header {
        flag_len: e1,
        stream_id: e2,
    };
    let flags = header.flags();
    if (FLAG_DATA != flags && FLAG_AUTH != flags) || 0 == header.len() {
        buf.advance(EVENT_HEADER_LEN);
        return Ok(Event {
            header: header,
            body: vec![],
            local: false,
        });
    }
    if buf.len() - EVENT_HEADER_LEN < header.len() as usize {
        return Err((
            header.len() + EVENT_HEADER_LEN as u32 - buf.len() as u32,
            "",
        ));
    }
    buf.advance(EVENT_HEADER_LEN);
    let dlen = header.len() as usize;
    let mut out = Vec::with_capacity(dlen);
    out.put_slice(&buf[0..dlen]);
    buf.advance(dlen);
    Ok(Event {
        header: header,
        body: out,
        local: false,
    })
}

pub fn chacha20poly1305_encrypt_event(ctx: &CryptoContext, ev: &Event, out: &mut BytesMut) {
    let mut sk: [u8; 10] = Default::default();
    sk[0..2].copy_from_slice(&ctx.key.as_bytes()[0..2]);
    sk[2..].copy_from_slice(&ctx.encrypt_nonce.to_le_bytes());
    let e1 = skip32::encode(&sk, ev.header.flag_len);
    let e2 = skip32::encode(&sk, ev.header.stream_id);
    out.put_u32_le(e1);
    out.put_u32_le(e2);

    if ev.body.len() > 0 {
        let key = chacha20poly1305::SecretKey::from_slice(&ctx.key.as_bytes()[0..32]).unwrap();
        let xnonce: u128 = ctx.encrypt_nonce as u128;
        let dlen = EVENT_HEADER_LEN + 16 + ev.body.len() as usize;
        out.reserve(dlen);
        unsafe {
            out.set_len(dlen);
        }
        let nonce = chacha20poly1305::Nonce::from_slice(&xnonce.to_le_bytes()[0..12]).unwrap();
        match chacha20poly1305::seal(
            &key,
            &nonce,
            &ev.body[..],
            None,
            &mut out[EVENT_HEADER_LEN..],
        ) {
            Ok(()) => {}
            Err(e) => {
                error!("encrypt error:{} {}", e, out.len());
            }
        }
    }
}

pub fn chacha20poly1305_decrypt_event(
    ctx: &CryptoContext,
    buf: &mut BytesMut,
) -> Result<Event, DecryptError> {
    if buf.len() < EVENT_HEADER_LEN {
        return Err((EVENT_HEADER_LEN as u32 - buf.len() as u32, ""));
    }
    let mut sk: [u8; 10] = Default::default();
    sk[0..2].copy_from_slice(&ctx.key.as_bytes()[0..2]);
    sk[2..].copy_from_slice(&ctx.decrypt_nonce.to_le_bytes());
    let mut xbuf: [u8; 4] = Default::default();
    xbuf.copy_from_slice(&buf[0..4]);
    let e1 = skip32::decode(&sk, u32::from_le_bytes(xbuf));
    xbuf.copy_from_slice(&buf[4..8]);
    let e2 = skip32::decode(&sk, u32::from_le_bytes(xbuf));

    let header = Header {
        flag_len: e1,
        stream_id: e2,
    };
    let flags = header.flags();
    if (FLAG_DATA != flags && FLAG_AUTH != flags) || 0 == header.len() {
        buf.advance(EVENT_HEADER_LEN);
        return Ok(Event {
            header: header,
            body: vec![],
            local: false,
        });
    }
    if buf.len() - EVENT_HEADER_LEN < (header.len() as usize + 16) {
        return Err((
            header.len() + EVENT_HEADER_LEN as u32 + 16 - buf.len() as u32,
            "",
        ));
    }
    buf.advance(EVENT_HEADER_LEN);
    let dlen = header.len() as usize;
    let mut out = Vec::with_capacity(dlen);
    unsafe {
        out.set_len(dlen);
    }
    let key = chacha20poly1305::SecretKey::from_slice(&ctx.key.as_bytes()[0..32]).unwrap();
    let xnonce: u128 = ctx.decrypt_nonce as u128;
    let nonce = chacha20poly1305::Nonce::from_slice(&xnonce.to_le_bytes()[0..12]).unwrap();
    match chacha20poly1305::open(&key, &nonce, &buf[0..dlen + 16], None, &mut out) {
        Ok(()) => {}
        Err(e) => {
            error!("decrypt error:{} {}", e, out.len());
            return Err((0, "Decrypt error"));
        }
    }
    buf.advance(dlen + 16);
    Ok(Event {
        header: header,
        body: out,
        local: false,
    })
}

impl CryptoContext {
    pub fn new(method: &str, k: &str, nonce: u64) -> Self {
        let mut key = String::from(k);
        while key.len() < 32 {
            key.push('F');
        }
        match method {
            METHOD_CHACHA20_POLY1305 => CryptoContext {
                key: key,
                encrypt_nonce: nonce,
                decrypt_nonce: nonce,
                encrypter: chacha20poly1305_encrypt_event,
                decrypter: chacha20poly1305_decrypt_event,
            },
            METHOD_NONE => CryptoContext {
                key: key,
                encrypt_nonce: nonce,
                decrypt_nonce: nonce,
                encrypter: none_encrypt_event,
                decrypter: none_decrypt_event,
            },
            _ => panic!("not supported crypto method."),
        }
    }
}

#[cfg(test)]
mod tests {
    // Note this useful idiom: importing names from outer (for mod tests) scope.
    use super::*;
    use std::str;
    #[test]
    fn test_crypto1() {
        let ev = new_fin_event(100, false);
        let mut ctx = CryptoContext::new(
            METHOD_CHACHA20_POLY1305,
            "21321321321321312321321321212asdfasdasdas1",
            21321312,
        );
        let mut buf = BytesMut::new();
        ctx.encrypt(&ev, &mut buf);
        println!("encoded buf len:{} {}", buf.capacity(), buf.len());

        let r = ctx.decrypt(&mut buf).unwrap();
        assert_eq!(r.header.stream_id, 100);
        //assert_eq!(r.header.flags(), FLAG_FIN);
        assert_eq!(r.header.len(), 0);
        assert_eq!(buf.len(), 0);
    }
    #[test]
    fn test_crypto2() {
        let s = "hello,world";
        let ev = new_data_event(100, s.as_bytes(), false);
        let mut ctx = CryptoContext::new(
            METHOD_CHACHA20_POLY1305,
            "21321321321321312321321321212asdfasdasdas1",
            21321312,
        );
        let mut buf = BytesMut::new();
        ctx.encrypt(&ev, &mut buf);
        println!(
            "encoded buf len:{} {} {} {}",
            buf.capacity(),
            buf.len(),
            ev.header.flag_len,
            ev.header.stream_id
        );

        let r = ctx.decrypt(&mut buf).unwrap();
        println!(
            "decode event len:{} {}",
            r.header.flag_len, r.header.stream_id
        );
        assert_eq!(r.header.stream_id, 100);
        assert_eq!(r.header.flags(), FLAG_DATA);
        assert_eq!(buf.len(), 0);
        assert_eq!(str::from_utf8(&r.body[..]).unwrap(), s);
    }

    #[test]
    fn test_crypto3() {
        let ev = new_fin_event(100, false);
        let mut ctx = CryptoContext::new(
            "none",
            "21321321321321312321321321212asdfasdasdas1",
            21321312,
        );
        let mut buf = BytesMut::new();
        ctx.encrypt(&ev, &mut buf);
        println!("encoded buf len:{} {}", buf.capacity(), buf.len());

        let r = ctx.decrypt(&mut buf).unwrap();
        assert_eq!(r.header.stream_id, 100);
        //assert_eq!(r.header.flags(), FLAG_FIN);
        assert_eq!(r.header.len(), 0);
        assert_eq!(buf.len(), 0);
    }
    #[test]
    fn test_crypto4() {
        let s = "hello,world";
        let ev = new_data_event(100, s.as_bytes(), false);
        let mut ctx = CryptoContext::new(
            "none",
            "21321321321321312321321321212asdfasdasdas1",
            21321312,
        );
        let mut buf = BytesMut::new();
        ctx.encrypt(&ev, &mut buf);
        println!(
            "encoded buf len:{} {} {} {}",
            buf.capacity(),
            buf.len(),
            ev.header.flag_len,
            ev.header.stream_id
        );

        let r = ctx.decrypt(&mut buf).unwrap();
        println!(
            "decode event len:{} {}",
            r.header.flag_len, r.header.stream_id
        );
        assert_eq!(r.header.stream_id, 100);
        assert_eq!(r.header.flags(), FLAG_DATA);
        assert_eq!(buf.len(), 0);
        assert_eq!(str::from_utf8(&r.body[..]).unwrap(), s);
    }

}
