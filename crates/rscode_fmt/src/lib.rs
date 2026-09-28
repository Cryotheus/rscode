// rough 
// Configure options for what formatter to use and optional sorting option 
//
//  this *must* support input and output using `proc-macro2`
//  do not directly transfer `syn` types across crate boundaries, however
//  they should be wrapped in another type if not already accompanied by other data
//
//  convenience functions for using strings should be offered so users of the crate do not need to add `proc-macro2` if they are only working with strings

#[derive(Debug, Clone)]
pub enum RsFormatter {
	//still not sure on offerring pretty-please
	PrettyPlease,
	RustFmt,
}
