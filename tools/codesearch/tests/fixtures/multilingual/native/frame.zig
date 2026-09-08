const std = @import("std");

/// 串口帧校验：逐字节异或得到校验和。
pub fn checksum(bytes: []const u8) u8 {
    var value: u8 = 0;
    for (bytes) |byte| value ^= byte;
    return value;
}

/// 收包缓冲区记录未消费的字节数。
pub const ReceiveBuffer = struct {
    used: usize,
    pub fn reset(self: *ReceiveBuffer) void {
        self.used = 0;
    }
};
