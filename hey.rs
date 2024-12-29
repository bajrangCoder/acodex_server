use std::io::{self, Write};

fn main() {
    // Get dimensions from the user
    let (width, height) = get_dimensions();

    // Draw the filled rectangle
    draw_filled_rectangle(width, height);
}

/// Function to get the dimensions of the rectangle from the user
fn get_dimensions() -> (usize, usize) {
    let mut input = String::new();

    // Prompt for width
    print!("Enter the width of the rectangle: ");
    io::stdout().flush().unwrap();
    io::stdin().read_line(&mut input).unwrap();
    let width = input.trim().parse::<usize>().unwrap_or(0);

    input.clear();

    // Prompt for height
    print!("Enter the height of the rectangle: ");
    io::stdout().flush().unwrap();
    io::stdin().read_line(&mut input).unwrap();
    let height = input.trim().parse::<usize>().unwrap_or(0);

    (width, height)
}

/// Function to draw a filled rectangle with no vertical gaps
fn draw_filled_rectangle(width: usize, height: usize) {
    if width == 0 || height == 0 {
        println!("Invalid dimensions. Width and height must be greater than 0.");
        return;
    }

    // Adjust height for the double-row effect
    let adjusted_height = (height + 1) / 2; // Each `▄` covers two rows
    let row = "▄".repeat(width); // Create a row with "▄"

    // Print the adjusted rectangle
    for _ in 0..adjusted_height {
        println!("{}", row);
    }
}
