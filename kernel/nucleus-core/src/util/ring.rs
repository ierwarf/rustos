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
        self.normalize_head();
        if self.len == CAPACITY {
            return false;
        }

        let tail = (self.head + self.len) % CAPACITY;
        self.data[tail] = Some(value);
        self.len += 1;
        true
    }

    pub fn push_overwrite(&mut self, value: T) {
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
