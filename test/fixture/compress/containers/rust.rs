pub struct Store {
    data: HashMap<String, u32>,
    name: String,
    dirty: bool,
}
