mod retriever;

fn main() {
    let query = "hash password";
    let index = vec![
        (String::from("1"), 0.9),
        (String::from("2"), 0.8),
        (String::from("3"), 0.7),
    ];
    let top_k = 0;

    let top_results = retriever::rank_core(query, index, top_k);

    println!("{:?}", top_results);
}