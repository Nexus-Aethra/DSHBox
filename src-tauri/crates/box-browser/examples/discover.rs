//! Print what browser discovery finds on this machine.
//!
//! Run with: cargo run -p box-browser --example discover

fn main() {
    let found = box_browser::discover();
    if found.is_empty() {
        println!("no Chromium-family browser found");
    } else {
        println!("{} candidate(s), in resolution order:", found.len());
        for (index, candidate) in found.iter().enumerate() {
            println!(
                "  {}. {:<9} {}",
                index + 1,
                candidate.kind.as_str(),
                candidate.path.display()
            );
        }
    }

    match box_browser::resolve(None) {
        Ok(chosen) => println!("\nresolved -> {} ({})", chosen.path.display(), chosen.kind.as_str()),
        Err(error) => println!("\nresolve failed: {error}"),
    }
}
