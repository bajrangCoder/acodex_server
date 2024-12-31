pub struct CircularBuffer {
    pub data: Vec<u8>,
    position: usize,
    max_size: usize,
}

impl CircularBuffer {
    pub fn new(max_size: usize) -> Self {
        Self {
            data: Vec::with_capacity(max_size),
            position: 0,
            max_size,
        }
    }

    pub fn write(&mut self, new_data: &[u8]) {
        for &byte in new_data {
            if self.data.len() < self.max_size {
                self.data.push(byte);
            } else {
                self.data[self.position] = byte;
                self.position = (self.position + 1) % self.max_size;
            }
        }
    }

    pub fn get_contents(&self) -> Vec<u8> {
        if self.data.len() < self.max_size {
            self.data.clone()
        } else {
            let mut result = Vec::with_capacity(self.max_size);
            result.extend_from_slice(&self.data[self.position..]);
            result.extend_from_slice(&self.data[..self.position]);
            result
        }
    }
}
