//! OneBot v11 与原生 REST 共享的图片 OCR 识别数据传输对象定义

use serde::{Deserialize, Serialize};

/// OCR 检测区域多边形的单个二维像素顶点坐标
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct OcrPoint {
    pub x: i32,
    pub y: i32,
}

/// OCR 识别出的单行文本内容、置信度及顺时针四角像素坐标
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct OcrTextDetection {
    pub text: String,
    pub confidence: u8,
    pub coordinates: Vec<OcrPoint>,
}

/// 单张图片执行本地 OCR 识别后的行级结果集合与实际使用语言标签
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct OcrImageResponse {
    pub texts: Vec<OcrTextDetection>,
    pub language: String,
}
