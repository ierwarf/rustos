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

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn remaining_capacity(&self) -> usize {
        CAPACITY - self.len
    }

    pub fn push(&mut self, value: T) -> bool {
        if CAPACITY == 0 {
            return false;
        }
        self.normalize_head();
        if self.len == CAPACITY {
            return false;
        }

        let tail = (self.head + self.len) % CAPACITY;
        self.data[tail] = Some(value);
        self.len += 1;
        true
    }

    pub fn push_front(&mut self, value: T) -> bool {
        if CAPACITY == 0 || self.len == CAPACITY {
            return false;
        }
        self.normalize_head();
        self.head = (self.head + CAPACITY - 1) % CAPACITY;
        self.data[self.head] = Some(value);
        self.len += 1;
        true
    }

    pub fn any(&self, mut predicate: impl FnMut(&T) -> bool) -> bool {
        for offset in 0..self.len {
            if let Some(value) = self.data[(self.head + offset) % CAPACITY].as_ref()
                && predicate(value)
            {
                return true;
            }
        }
        false
    }

    pub fn push_overwrite(&mut self, value: T) {
        if CAPACITY == 0 {
            return;
        }
        self.normalize_head();
        if self.len == CAPACITY {
            self.data[self.head] = None;
            self.head = (self.head + 1) % CAPACITY;
            self.len -= 1;
            self.normalize_head();
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
        self.normalize_head();
        if self.len == 0 {
            return None;
        }

        let value = self.data[self.head].take();
        self.head = (self.head + 1) % CAPACITY;
        self.len -= 1;
        value
    }

    pub fn peek(&self) -> Option<T> {
        if self.len == 0 {
            return None;
        }

        let mut offset = 0usize;
        while offset < self.len {
            if let Some(value) = self.data[(self.head + offset) % CAPACITY] {
                return Some(value);
            }
            offset += 1;
        }
        None
    }

    pub fn pop_into(&mut self, dest: &mut [T]) -> usize {
        let mut count = 0;
        for slot in dest.iter_mut() {
            let Some(value) = self.pop() else {
                break;
            };
            *slot = value;
            count += 1;
        }
        count
    }

    pub fn copy_into(&self, dest: &mut [T]) -> usize {
        let mut count = 0;
        for index in 0..min(dest.len(), self.len) {
            let Some(value) = self.data[(self.head + index) % CAPACITY] else {
                break;
            };
            dest[index] = value;
            count += 1;
        }
        count
    }

    fn normalize_head(&mut self) {
        while self.len != 0 && self.data[self.head].is_none() {
            self.head = (self.head + 1) % CAPACITY;
            self.len -= 1;
        }
    }
}

impl<T: Copy, const CAPACITY: usize> Default for RingBuffer<T, CAPACITY> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::RingBuffer;

    #[test]
    fn zero_capacity_is_total_and_never_indexes_or_divides() {
        let mut ring = RingBuffer::<u8, 0>::new();
        assert!(!ring.push(1));
        ring.push_overwrite(2);
        assert_eq!(ring.extend_overwrite(&[3, 4]), 2);
        assert_eq!(ring.pop(), None);
        assert_eq!(ring.peek(), None);
        assert_eq!(ring.remaining_capacity(), 0);
    }

    #[test]
    fn overwrite_preserves_the_newest_wrapped_values() {
        let mut ring = RingBuffer::<u8, 3>::new();
        assert_eq!(ring.extend_overwrite(&[1, 2, 3, 4, 5]), 5);
        let mut output = [0_u8; 3];
        assert_eq!(ring.pop_into(&mut output), 3);
        assert_eq!(output, [3, 4, 5]);
        assert!(ring.is_empty());
    }

    #[test]
    fn push_front_preserves_retry_order_and_capacity() {
        let mut ring = RingBuffer::<u8, 3>::new();
        assert!(ring.push(2));
        assert!(ring.push(3));
        assert!(ring.push_front(1));
        assert!(!ring.push_front(0));
        assert!(ring.any(|value| *value == 2));
        assert!(!ring.any(|value| *value == 4));
        assert_eq!(ring.pop(), Some(1));
        assert_eq!(ring.pop(), Some(2));
        assert_eq!(ring.pop(), Some(3));
    }

    #[test]
    fn private_retry_slot_survives_a_racing_public_admission() {
        const PUBLIC_CAPACITY: usize = 3;
        let mut ring = RingBuffer::<u8, 4>::new();
        for value in 1..=PUBLIC_CAPACITY as u8 {
            assert!(ring.push(value));
        }
        let retry = ring.pop().expect("dequeue retry owner");
        assert!(ring.push(4), "producer may refill the public capacity");
        assert_eq!(ring.len(), PUBLIC_CAPACITY);
        assert!(ring.push_front(retry), "private slot preserves exact retry");
        assert_eq!(ring.len(), PUBLIC_CAPACITY + 1);
        assert!(!ring.push(5));
    }
}
