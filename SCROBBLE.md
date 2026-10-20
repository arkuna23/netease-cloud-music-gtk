# 真实播放记录同步

## 使用

首选项 → Music → **同步听歌记录**（`sync-listening-history`，默认开启）。登录后按真实播放过程同步：开始事件 `_plv`，结束事件 `_pld`。设置关闭时清理待发送事件；再次开启后从下一段可确认的播放重新计时。

- API fork：<https://github.com/arkuna23/netease-cloud-music-api>
- 固定 revision：`04a2aac55c00c1b355a49b89a2a4e1abb5d73b4d`
- 客户端实施基线：`225d14fb42bc0e0fdce1cbcb640577c307efc641`
- 协议来源、编码细节和最小示例：API 仓库的 `SCROBBLE.md`。

## 记录和发送规则

`application.rs` 管理加载及账号 generation；`PlayerControls` 在 URI 加载边界给 GStreamer 观察事件标记媒体 generation；`audio/scrobble.rs` 管理计时、会话和后台队列。

计时取单调时间差与媒体位置正向变化的较小值。Playing 状态下出现真实进度才确认开始。暂停、缓冲和 seek 都建立新的检查点；超过 5 秒的采样间隔采用重建检查点策略。媒体总时长、进度和实际播放时长分别维护，现有微秒事件在边界转换为 `Duration`。

暂停恢复延续会话，循环和停止后重新播放建立新会话。歌曲缓存保留网易云歌曲 ID；本地音频依据可识别 ID 决定记录资格。歌单、专辑和榜单入口携带真实来源，未知来源保留为空；队列保存来源元数据并兼容既有播放列表文件。

发送队列为 128 个事件预留空间（最多 64 组开始/结束事件），单消费者按顺序发送，每个事件一次网络尝试。队列满时优先释放尚未开始发送的最旧整组；其余会话均已开始发送时，当前新会话采用终止状态。结果未知与已确认分别记录在脱敏日志中。

关闭同步或退出登录会清理待发送事件；进行中的请求使用原账号冻结上下文。真正退出应用时结算并最多等待 3 秒，窗口转后台时继续记录。强制终止的交付边界为内存队列中最后一批事件；首版范围为在线同步。

## 检查结果与复现

核对日期：2026-10-20。

| 检查 | 结果 |
|---|---|
| API NCBL / 请求 / 认证快照测试 | 3 项通过 |
| 客户端计时、会话、账号与队列测试 | 7 项通过 |
| GTK 缓存音频播放、EOS、重新播放及首选项双向绑定 | 通过 |
| GTK 关闭窗口转后台、真正退出 | 通过 |

本机安装 GTK4、libadwaita、GStreamer 和项目开发依赖后，执行：

```sh
meson setup _build --buildtype=debug
meson compile -C _build
cargo test audio::scrobble
```

两个 GTK 冒烟用例各自在独立进程运行，使用有效显示会话（无显示环境时可在 Xvfb 中运行）。测试在隔离目录中使用内存 GSettings：

```sh
glib-compile-schemas --strict --targetdir=_build/data data
(
  export GSETTINGS_SCHEMA_DIR="$PWD/_build/data"
  export GSETTINGS_BACKEND=memory
  export XDG_CACHE_HOME="$PWD/_build/smoke/cache"
  export XDG_CONFIG_HOME="$PWD/_build/smoke/config"
  export XDG_DATA_HOME="$PWD/_build/smoke/data"
  cargo test cached_playback_emits_measured_generation_events -- --ignored --test-threads=1
  cargo test window_close_preserves_background_then_uses_graceful_quit -- --ignored --test-threads=1
)
```

`cargo check`、Meson debug 构建、变更文件 rustfmt 检查和 `git diff --check` 结果随本次交付记录。Clippy 风格提示保留原项目约定。

## 官方统计验收

状态：**实现完成，入账待验证**。

使用自己的已登录账号，记录歌曲 ID、客户端/API revision、真实播放起止时间、暂停及 seek 操作、脱敏上传结果，并分别核对以下官方端统计：

| 观察时间 | 最近播放 | 听歌排行 | 累计听歌时长 | 官方端截图或备注 |
|---|---|---|---|---|
| 播放前 | | | | |
| 播放结束 | | | | |
| 15 分钟后 | | | | |
| 次日 | | | | |

上传响应确认表示文件接收，三类统计各自以官方端证据验收。服务端统计的生效范围和延迟以账号实测为准。诊断日志通过 `RUST_LOG=netease_cloud_music_gtk4=debug` 开启；请仅分享脱敏的会话、事件类型和状态。
