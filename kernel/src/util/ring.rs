use core::cmp::min;

pub struct RingBuffer<T: Copy, const CAPACITY: usize> {
    data: [Option<T>; CAPACITY],
    head: usize,
    len: usize,
}

impl<T: Copy, const CAPACITY: usize> RingBuffer<T, CAPACITY> {
    pub const fn new() -> Self {
        Self {
            data: [None; CAPACITY],
            head: 0,
            len: 0,
        }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn remaining_capacity(&self) -> usize {
        CAPACITY - self.len
    }

    pub fn push(&mut self, value: T) -> bool {
        if self.len == CAPACITY {
            return false;
        }

        let tail = (self.head + self.len) % CAPACITY;
        self.data[tail] = Some(value);
        self.len += 1;
        true
    }

    pub fn push_overwrite(&mut self, value: T) {
        if self.len == CAPACITY {
            self.data[self.head] = None;
            self.head = (self.head + 1) % CAPACITY;
            self.len -= 1;
        }

        let tail = (self.head + self.len) % CAPACITY;
        self.data[tail] = Some(value);
        self.len += 1;
    }

    pub fn extend_overwrite(&mut self, values: &[T]) -> usize {
        for &value in values {
            self.push_overwrite(value);
        }
        values.len()
    }

    pub fn pop(&mut self) -> Option<T> {
        if self.len == 0 {
            return None;
        }

        let value = self.data[self.head].take();
        self.head = (self.head + 1) % CAPACITY;
        self.len -= 1;
        value
    }

    pub fn pop_into(&mut self, dest: &mut [T]) -> usize {
        let count = min(dest.len(), self.len);
        for slot in dest.iter_mut().take(count) {
            *slot = self.pop().expect("ring buffer length must match stored entries");
        }
        count
    }

    pub fn copy_into(&self, dest: &mut [T]) -> usize {
        let count = min(dest.len(), self.len);
        for (index, slot) in dest.iter_mut().take(count).enumerate() {
            *slot = self.data[(self.head + index) % CAPACITY]
                .expect("ring buffer length must match stored entries");
        }
        count
    }
}

#[cfg(test)]
mod tests {
    use super::RingBuffer;

    #[test]
    fn preserves_fifo_order() {
        let mut ring = RingBuffer::<u8, 4>::new();
        assert!(ring.push(b'a'));
        assert!(ring.push(b'b'));
        assert!(ring.push(b'c'));

        let mut out = [0_u8; 2];
        assert_eq!(ring.pop_into(&mut out), 2);
        assert_eq!(&out, b"ab");
        assert_eq!(ring.pop(), Some(b'c'));
        assert_eq!(ring.pop(), None);
    }

    #[test]
    fn overwrite_keeps_most_recent_values() {
        let mut ring = RingBuffer::<u8, 4>::new();
        assert_eq!(ring.extend_overwrite(b"abcdef"), 6);

        let mut out = [0_u8; 4];
        assert_eq!(ring.copy_into(&mut out), 4);
        assert_eq!(&out, b"cdef");
    }
}
