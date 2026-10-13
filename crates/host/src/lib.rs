//! The nocomputer host process. The capture app spawns it and feeds it encoded video
//! over a local pipe (see [`local`]); it serves paired viewers with `nocomputer-proto`.

pub mod h264;
pub mod local;
