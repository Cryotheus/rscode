//! Module file resolution.

mod a;
mod b;
#[path = "other/o.rs"]
mod o;
mod m {
	#[path = "pp.rs"]
	mod pp;
	mod q;
}
#[path = "p"]
mod pdir {
	mod c;
}
mod both;
mod missing;
#[cfg(any())]
mod missing_inactive;
#[cfg(feature = "maybe")]
mod missing_maybe;
#[cfg_attr(all(), path = "via_cfg_attr.rs")]
mod ca;
#[cfg_attr(feature = "x", path = "feature_x.rs")]
mod fx;
#[path = "first.rs"]
#[path = "second.rs"]
mod multi;
mod r#type;
#[path = "cycle_a.rs"]
mod cycle;
mod broken;
#[path = "shared.rs"]
mod shared1;
#[path = "shared.rs"]
mod shared2;
#[cfg(any())]
mod inactive {
	mod nested_missing;
}
#[path = "../outside/up.rs"]
mod up;
