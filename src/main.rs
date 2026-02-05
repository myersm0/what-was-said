use anyhow::Result;

fn main() -> Result<()> {
	let connection = rusqlite::Connection::open("cathedrals.db")?;
	cathedrals::storage::initialize(&connection)?;
	println!("initialized database");
	Ok(())
}
