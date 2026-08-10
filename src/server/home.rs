use log::trace;

pub async fn main() -> &'static str {
	trace!("Received request: home");
	"Home Page"
}
