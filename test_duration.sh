#!/bin/bash

# 测试脚本：验证变速+插入空白后的音频时长是否与原始音频一致

echo "=== 测试变速+插入空白逻辑 ==="

# 检查是否有输出文件
if [ ! -d "output" ]; then
    echo "错误: output 目录不存在"
    exit 1
fi

# 查找最新的输出目录
LATEST_OUTPUT=$(find output -type d -mindepth 1 -maxdepth 1 | sort | tail -1)

if [ -z "$LATEST_OUTPUT" ]; then
    echo "错误: 找不到输出目录"
    exit 1
fi

echo "找到输出目录: $LATEST_OUTPUT"

# 查找原始音频和变速音频
ORIGINAL_AUDIO=""
TIMED_AUDIO=""

# 查找原始音频（可能是 .wav 文件，但不是 _timed.wav）
for file in "$LATEST_OUTPUT"/*.wav; do
    if [[ "$file" != *"_timed.wav" ]]; then
        ORIGINAL_AUDIO="$file"
        break
    fi
done

# 查找变速音频
TIMED_AUDIO=$(find "$LATEST_OUTPUT" -name "*_timed.wav" | head -1)

if [ -z "$ORIGINAL_AUDIO" ] || [ ! -f "$ORIGINAL_AUDIO" ]; then
    echo "错误: 找不到原始音频文件"
    exit 1
fi

if [ -z "$TIMED_AUDIO" ] || [ ! -f "$TIMED_AUDIO" ]; then
    echo "错误: 找不到变速音频文件"
    exit 1
fi

echo "原始音频: $ORIGINAL_AUDIO"
echo "变速音频: $TIMED_AUDIO"

# 获取原始音频时长
ORIGINAL_DURATION=$(ffprobe -v error -show_entries format=duration -of default=noprint_wrappers=1:nokey=1 "$ORIGINAL_AUDIO" 2>/dev/null)
TIMED_DURATION=$(ffprobe -v error -show_entries format=duration -of default=noprint_wrappers=1:nokey=1 "$TIMED_AUDIO" 2>/dev/null)

if [ -z "$ORIGINAL_DURATION" ]; then
    echo "错误: 无法获取原始音频时长"
    exit 1
fi

if [ -z "$TIMED_DURATION" ]; then
    echo "错误: 无法获取变速音频时长"
    exit 1
fi

echo ""
echo "原始音频时长: ${ORIGINAL_DURATION} 秒"
echo "变速音频时长: ${TIMED_DURATION} 秒"

# 计算差值
DIFF=$(echo "$ORIGINAL_DURATION - $TIMED_DURATION" | bc | sed 's/^-//')
DIFF_MS=$(echo "$DIFF * 1000" | bc | cut -d. -f1)

echo "时长差值: ${DIFF} 秒 (${DIFF_MS} 毫秒)"

# 允许的误差范围（100毫秒）
TOLERANCE_MS=100

if [ -z "$DIFF_MS" ] || [ "$DIFF_MS" -gt "$TOLERANCE_MS" ]; then
    echo ""
    echo "❌ 测试失败: 时长不一致！差值超过允许范围（${TOLERANCE_MS}ms）"
    exit 1
else
    echo ""
    echo "✅ 测试通过: 时长一致（差值在允许范围内）"
    exit 0
fi
