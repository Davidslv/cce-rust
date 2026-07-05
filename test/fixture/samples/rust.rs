use std::collections::HashMap;

pub fn build_index() -> HashMap<String, u32> {
    HashMap::new()
}

pub struct Store {
    data: HashMap<String, u32>,
}

impl Store {
    pub fn get(&self, key: &str) -> u32 {
        0
    }
}
