# 局域网传输协议 v2

TCP 传输 + UDP 广播发现。每个 TCP 动作一条短连接。所有整数大端，字符串 = `u16 长度 + UTF-8`。

v2 与 v1 **不兼容**：设备ID由 16 字节随机数改为 32 字节 Ed25519 公钥，握手增加签名验证。

## 1. 帧格式

```
[len: u32][ty: u8][payload: len 字节]
```

`len` 只计 payload。接收方在读 payload 之前先检查上限：

| 场景 | 上限 |
|---|---|
| 握手、好友请求、各种应答 | 4 KiB |
| 已是好友发来的文字 / 文件清单 | 16 MiB |
| 文件数据帧 | 1 MiB（发送端实际用 64 KiB） |

陌生人（非好友）在握手后只允许发 4 KiB 以内的帧。

## 2. 帧类型

| ty | 名称 | payload |
|---|---|---|
| 0x01 | HELLO | `version:u8=2` `id:32字节(公钥)` `listen_port:u16` `name:str` `nonce:16字节` |
| 0x05 | AUTH | Ed25519 签名，64 字节 |
| 0x02 | FRIEND_REQUEST | 空 |
| 0x03 | FRIEND_ACCEPT | 空 |
| 0x04 | FRIEND_REJECT | 空 |
| 0x10 | TEXT | UTF-8 原文，≤ 1 MiB |
| 0x20 | OFFER | 清单（同 v1） |
| 0x21 | OFFER_ACCEPT | 空 |
| 0x22 | OFFER_REJECT | 空 |
| 0x23 | FILE_DATA | 原始文件字节 |
| 0x24 | DONE | 空 |
| 0x30 | ACK | 空（成功） |
| 0xFF | ERROR | `code:u16` `msg:str` |

错误码：1 未加好友 · 3 请求不合法 · 6 路径被拒 · 8 内部错误 · 9 身份验证失败。

## 3. 握手与身份验证

```
客户端 → 服务端  HELLO(id_c, nonce_c, …)
客户端 ← 服务端  HELLO(id_s, nonce_s, …)
客户端 → 服务端  AUTH = Sign(sk_c, auth_msg('C', nonce_c, nonce_s, id_c, id_s))
                 服务端验证（用 id_c 当公钥），失败回 ERROR(9) 并断开
客户端 ← 服务端  AUTH = Sign(sk_s, auth_msg('S', nonce_c, nonce_s, id_c, id_s))
                 客户端验证（用 id_s 当公钥），失败断开
客户端 → 服务端  请求帧（FRIEND_REQUEST / TEXT / OFFER）
```

```
auth_msg(role, nonce_c, nonce_s, id_c, id_s) =
    "lanchat-auth-v2" ‖ role(1 字节) ‖ nonce_c ‖ nonce_s ‖ id_c ‖ id_s
```

- 签名绑定了双方的随机数和ID，不能重放到别的连接，也不能把对方的签名反射回去。
- **设备ID就是公钥**。只知道别人的设备ID并不能冒充他，因为没有私钥签不出 AUTH。
- 私钥只保存在本机（`identity.txt`），绝不发送。

## 4. 好友规则

好友记录 = `设备ID + 名字 + IP + 端口`，**只按设备ID识别**。

- 已验证身份的好友再次连入，或我方连出时，名字、IP、端口有变化就**自动更新记录**；好友关系不受影响。
- 外连时设备ID对不上（该地址上换了别的设备）→ 报错，不发送任何内容，也不修改记录。
- 已是好友的重复 FRIEND_REQUEST 直接同意。
- 好友请求同意时，同意方先保存对方，再回 ACCEPT；发起方收到 ACCEPT 后保存。

### 加好友

```
A → B  (握手，双方身份均已验证)
A → B  FRIEND_REQUEST          （B 界面弹出确认，最长等 120 秒）
A ← B  FRIEND_ACCEPT / FRIEND_REJECT
```

### 发文字 / 文件

同 v1：`TEXT → ACK`；`OFFER → OFFER_ACCEPT/REJECT → FILE_DATA×N → DONE → ACK`。

## 5. 局域网发现（UDP 广播）

固定 UDP 端口 **47320**（所有设备一致）。数据报：

```
"LCDV"(4) ‖ version:u8=2 ‖ kind:u8 ‖ id:32字节 ‖ tcp_port:u16 ‖ name:str
kind: 1 = ANNOUNCE（我在这）  2 = QUERY（谁在？收到的人单播回 ANNOUNCE）
```

- 每 8 秒向每个网卡所在子网的广播地址和 255.255.255.255 各发一次 ANNOUNCE（"允许被发现"关闭时不发、不回应）。
- 多网卡时，对每个本机 IPv4 地址各开一个绑定到该地址的 socket 发送，保证每个网络都收到。
- 接收方把 ANNOUNCE 记入"发现列表"，30 秒没再收到就过期；包里没有 IP，用数据报的来源地址。
- **UDP 包不可信**（可被伪造），所以：
  - 发现列表仅用于展示；真正加好友仍走带签名的 TCP 握手。
  - 已有好友的 IP/端口/名字变了：先对新地址做一次完整 TCP 握手验证设备ID，通过才更新记录（同一好友 15 秒内最多验证一次）。
- 广播可能被防火墙、AP 隔离、VLAN 挡掉，所以手动输入 `IP:端口` 加好友始终保留。

## 6. 接收端安全要求

同 v1：路径逐段校验（拒绝 `..`、绝对路径、`\`、`:`、控制字符；Windows 额外拒绝保留名和非法字符）、同一根目录、无重复、重名自动改名绝不覆盖（`create_new`）、先校验再弹窗、数据长度必须与清单一致、并发连接上限 32。

## 7. 本地数据

| 文件 | 内容 |
|---|---|
| `identity.txt` | `id`(公钥)、`secret`(私钥，Unix 下权限 0600)、`name`、`port` |
| `friends.tsv` | `设备ID \t 名字 \t IP \t 端口`，v1 的旧记录会被丢弃，需重新添加 |
| `history/<设备ID>.log` | 每行 `毫秒时间戳 \t M/P/S \t 转义后的正文`；M=我发的 P=对方发的 S=系统提示 |
| `gui.txt` | 接收目录、是否允许被发现、免打扰 |
| `logs/lanchat.log` | GUI 启动与运行日志（超过 512 KB 轮转） |

数据目录：Windows `%APPDATA%\lan-transfer`，Linux `~/.config/lan-transfer`。

## 8. 已知局限

- **没有通道加密**：身份是验证过的，但内容仍是明文，局域网内能抓包的人能看到。需要的话下一步用 Noise 或 TLS 加密通道。
- 符号链接被跳过；仅 IPv4；不支持断点续传。
