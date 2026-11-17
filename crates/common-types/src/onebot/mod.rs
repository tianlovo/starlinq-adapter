//! OneBot v11 协议实体、全量消息段、表情表与持久化 DTO 门面

pub mod face_table;
pub mod friend_dtos;
pub mod group_dtos;
pub mod id_hasher;
pub mod ocr_dtos;
pub mod segment_codec;
pub mod segments;
pub mod storage_dtos;

pub use face_table::*;
pub use friend_dtos::*;
pub use group_dtos::*;
pub use id_hasher::*;
pub use ocr_dtos::*;
pub use segment_codec::*;
pub use segments::*;
pub use storage_dtos::*;
