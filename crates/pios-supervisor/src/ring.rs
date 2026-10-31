//! Fixed-capacity byte ring holding the tail of a service's output.

use std::collections::VecDeque;

pub struct Ring {
    buf: VecDeque<u8>,
    cap: usize,
}

impl Ring {
    pub fn new(cap: usize) -> Self {
        Self { buf: VecDeque::with_capacity(cap.min(4096)), cap }
    }

    pub fn push(&mut self, data: &[u8]) {
        let data = if data.len() > self.cap { &data[data.len() - self.cap..] } else { data };
        let overflow = (self.buf.len() + data.len()).saturating_sub(self.cap);
        self.buf.drain(..overflow);
        self.buf.extend(data);
    }

    pub fn text(&self) -> String {
        let (a, b) = self.buf.as_slices();
        let mut v = Vec::with_capacity(a.len() + b.len());
        v.extend_from_slice(a);
        v.extend_from_slice(b);
        String::from_utf8_lossy(&v).into_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_tail() {
        let mut r = Ring::new(8);
        r.push(b"hello ");
        r.push(b"world!");
        assert_eq!(r.text(), "o world!");
        r.push(b"0123456789");
        assert_eq!(r.text(), "23456789");
    }
}
