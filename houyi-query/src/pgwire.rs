use anyhow::{anyhow, Result};
use std::collections::HashMap;

pub const PROTOCOL_VERSION_3: i32 = 196608;
pub const SSL_REQUEST_CODE: i32 = 80877103;
/// GSSAPI 加密请求（P4.2.2 起在 TLS 协商中显式回 'N'；不支持 GSSAPI）。
/// EN: GSSAPI encryption request code — always answered 'N' (unsupported).
pub const GSSENC_REQUEST_CODE: i32 = 80877104;

#[derive(Debug, Clone, PartialEq)]
pub struct StartupMessage {
    pub protocol_version: i32,
    pub params: HashMap<String, String>,
}

/// 前端消息：Simple Query（Q）与扩展查询子集（P/B/D/E/S/H/C）+ Terminate（X）。
/// Frontend messages: Simple Query plus a minimal extended-query subset.
#[derive(Debug, Clone, PartialEq)]
pub enum BindParamValue {
    Text(String),
    Binary(Vec<u8>),
}

impl BindParamValue {
    pub fn encoded_len(&self) -> usize {
        match self {
            Self::Text(s) => s.len(),
            Self::Binary(bytes) => bytes.len(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum FrontendMessage {
    Query(String),
    /// Parse：仅支持空语句名；记录 `num_params` 供 Bind/Describe 校验。
    /// EN: Parse: only empty statement names are supported; records `num_params` for Bind/Describe checks.
    Parse {
        name: String,
        query: String,
        num_params: usize,
        param_oids: Vec<u32>,
    },
    /// Bind：支持 0/1 个文本参数；门户/语句名可为空（未命名）。
    /// EN: Bind: supports 0/1 text parameters; portal/statement names may be empty (unnamed).
    Bind {
        portal: String,
        statement: String,
        params: Vec<BindParamValue>,
        param_formats: Vec<i16>,
        result_formats: Vec<i16>,
    },
    /// Execute：门户名可为空。
    /// EN: Execute: the portal name may be empty.
    Execute {
        portal: String,
        max_rows: u32,
    },
    /// Describe：`kind` 为 `S`（Statement）或 `P`（Portal）。
    /// EN: Describe: `kind` is `S` (Statement) or `P` (Portal).
    Describe {
        kind: u8,
        name: String,
    },
    /// Close：同上。
    /// EN: Close: same as above.
    Close {
        kind: u8,
        name: String,
    },
    Sync,
    /// Flush：无响应报文，仅触发输出刷写。
    /// EN: Flush: no response message; only triggers output flushing.
    Flush,
    /// CopyData（前端 tag 'd'）— 当前实现不支持 COPY 流。
    /// EN: CopyData (frontend tag 'd') — COPY streams are not supported.
    CopyData,
    /// CopyDone（前端 tag 'c'）— 当前实现不支持 COPY 流。
    /// EN: CopyDone (frontend tag 'c') — COPY streams are not supported.
    CopyDone,
    /// CopyFail（前端 tag 'f'）— 当前实现不支持 COPY 流，保留客户端错误文本便于诊断。
    /// EN: CopyFail (frontend tag 'f') — COPY unsupported; keeps client error text for diagnosis.
    CopyFail(String),
    Terminate,
    Unsupported(u8),
}

pub fn parse_startup_message(payload: &[u8]) -> Result<StartupMessage> {
    if payload.len() < 4 {
        return Err(anyhow!("startup payload too short"));
    }
    let protocol_version = i32::from_be_bytes(
        payload[0..4]
            .try_into()
            .map_err(|_| anyhow!("invalid protocol bytes"))?,
    );
    let mut params = HashMap::new();
    let mut idx = 4usize;
    let mut parts = Vec::new();
    while idx < payload.len() {
        let Some(end) = payload[idx..].iter().position(|b| *b == 0) else {
            return Err(anyhow!("startup payload missing null terminator"));
        };
        let token = &payload[idx..idx + end];
        idx += end + 1;
        if token.is_empty() {
            break;
        }
        let s = String::from_utf8(token.to_vec())?;
        parts.push(s);
    }
    let mut it = parts.into_iter();
    while let Some(k) = it.next() {
        let Some(v) = it.next() else {
            break;
        };
        params.insert(k, v);
    }
    Ok(StartupMessage {
        protocol_version,
        params,
    })
}

pub fn decode_startup_packet(packet: &[u8]) -> Result<StartupMessage> {
    if packet.len() < 8 {
        return Err(anyhow!("startup packet too short"));
    }
    let len = i32::from_be_bytes(packet[0..4].try_into().map_err(|_| anyhow!("bad len"))?) as usize;
    if len != packet.len() {
        return Err(anyhow!("startup packet length mismatch"));
    }
    parse_startup_message(&packet[4..])
}

pub fn encode_authentication_ok() -> Vec<u8> {
    let mut out = Vec::with_capacity(9);
    out.push(b'R');
    out.extend_from_slice(&8_i32.to_be_bytes());
    out.extend_from_slice(&0_i32.to_be_bytes());
    out
}

/// AuthenticationCleartextPassword 质询（'R' + int32(3)）：要求客户端回
/// PasswordMessage。beta 简化认证——不实现 SCRAM;明文密码仅在 TLS 或
/// localhost 部署下安全（P4.2.2 提供 TLS）。
/// EN: Cleartext-password challenge. SCRAM is out of scope for beta; use
/// only over TLS or localhost.
pub fn encode_authentication_cleartext_password() -> Vec<u8> {
    let mut out = Vec::with_capacity(9);
    out.push(b'R');
    out.extend_from_slice(&8_i32.to_be_bytes());
    out.extend_from_slice(&3_i32.to_be_bytes());
    out
}

pub fn encode_parameter_status(name: &str, value: &str) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.extend_from_slice(name.as_bytes());
    payload.push(0);
    payload.extend_from_slice(value.as_bytes());
    payload.push(0);
    let len = (payload.len() + 4) as i32;

    let mut out = Vec::new();
    out.push(b'S');
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(&payload);
    out
}

pub fn encode_ready_for_query() -> Vec<u8> {
    let mut out = Vec::with_capacity(6);
    out.push(b'Z');
    out.extend_from_slice(&5_i32.to_be_bytes());
    out.push(b'I');
    out
}

pub fn encode_command_complete(tag: &str) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.extend_from_slice(tag.as_bytes());
    payload.push(0);
    let len = (payload.len() + 4) as i32;
    let mut out = Vec::new();
    out.push(b'C');
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(&payload);
    out
}

pub fn encode_row_description_single_text(column_name: &str) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&1_i16.to_be_bytes()); // field count
    payload.extend_from_slice(column_name.as_bytes());
    payload.push(0); // field name terminator
    payload.extend_from_slice(&0_i32.to_be_bytes()); // table oid
    payload.extend_from_slice(&0_i16.to_be_bytes()); // attr num
    payload.extend_from_slice(&25_i32.to_be_bytes()); // TEXTOID
    payload.extend_from_slice(&(-1_i16).to_be_bytes()); // type size
    payload.extend_from_slice(&0_i32.to_be_bytes()); // type modifier
    payload.extend_from_slice(&0_i16.to_be_bytes()); // text format

    let len = (payload.len() + 4) as i32;
    let mut out = Vec::new();
    out.push(b'T');
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(&payload);
    out
}

pub fn encode_data_row_single_text(value: &str) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&1_i16.to_be_bytes()); // column count
    payload.extend_from_slice(&(value.len() as i32).to_be_bytes());
    payload.extend_from_slice(value.as_bytes());

    let len = (payload.len() + 4) as i32;
    let mut out = Vec::new();
    out.push(b'D');
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(&payload);
    out
}

pub fn encode_error_response(severity: &str, code: &str, message: &str) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.push(b'S');
    payload.extend_from_slice(severity.as_bytes());
    payload.push(0);
    payload.push(b'C');
    payload.extend_from_slice(code.as_bytes());
    payload.push(0);
    payload.push(b'M');
    payload.extend_from_slice(message.as_bytes());
    payload.push(0);
    payload.push(0); // end of fields

    let len = (payload.len() + 4) as i32;
    let mut out = Vec::new();
    out.push(b'E');
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(&payload);
    out
}

/// ParseComplete（'1'）。
pub fn encode_parse_complete() -> Vec<u8> {
    let mut out = Vec::with_capacity(5);
    out.push(b'1');
    out.extend_from_slice(&4_i32.to_be_bytes());
    out
}

/// BindComplete（'2'）。
pub fn encode_bind_complete() -> Vec<u8> {
    let mut out = Vec::with_capacity(5);
    out.push(b'2');
    out.extend_from_slice(&4_i32.to_be_bytes());
    out
}

/// CloseComplete（'3'）。
pub fn encode_close_complete() -> Vec<u8> {
    let mut out = Vec::with_capacity(5);
    out.push(b'3');
    out.extend_from_slice(&4_i32.to_be_bytes());
    out
}

/// PortalSuspended（'s'）消息。
/// EN: PortalSuspended ('s') message.
pub fn encode_portal_suspended() -> Vec<u8> {
    let mut out = Vec::with_capacity(5);
    out.push(b's');
    out.extend_from_slice(&4_i32.to_be_bytes());
    out
}

/// ParameterDescription（'t'），参数类型统一填 0（unknown），仅表达参数个数。
/// EN: ParameterDescription ('t'); all parameter types are 0 (unknown); only the count is expressed.
pub fn encode_parameter_description(param_count: i16) -> Vec<u8> {
    let mut out = Vec::with_capacity(8);
    out.push(b't');
    let len = 4 + 2 + (param_count.max(0) as usize) * 4;
    out.extend_from_slice(&(len as i32).to_be_bytes());
    out.extend_from_slice(&param_count.to_be_bytes());
    for _ in 0..param_count.max(0) {
        out.extend_from_slice(&0_i32.to_be_bytes());
    }
    out
}

/// ParameterDescription（'t'），按给定 OID 列表编码参数类型。
/// EN: ParameterDescription ('t'); encodes parameter types from the given OID list.
pub fn encode_parameter_description_oids(param_oids: &[u32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + param_oids.len() * 4);
    out.push(b't');
    let len = 4 + 2 + param_oids.len() * 4;
    out.extend_from_slice(&(len as i32).to_be_bytes());
    out.extend_from_slice(&(param_oids.len() as i16).to_be_bytes());
    for oid in param_oids {
        out.extend_from_slice(&(*oid as i32).to_be_bytes());
    }
    out
}

/// ParameterDescription（'t'），零参数。
/// EN: ParameterDescription ('t') with zero parameters.
pub fn encode_parameter_description_empty() -> Vec<u8> {
    encode_parameter_description(0)
}

pub fn encode_notice_response(severity: &str, message: &str) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.push(b'S');
    payload.extend_from_slice(severity.as_bytes());
    payload.push(0);
    payload.push(b'M');
    payload.extend_from_slice(message.as_bytes());
    payload.push(0);
    payload.push(0); // end of fields

    let len = (payload.len() + 4) as i32;
    let mut out = Vec::new();
    out.push(b'N');
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(&payload);
    out
}

fn read_cstring(buf: &[u8]) -> Result<(&str, &[u8])> {
    let pos = buf
        .iter()
        .position(|&b| b == 0)
        .ok_or_else(|| anyhow!("cstring missing null"))?;
    let s = std::str::from_utf8(&buf[..pos]).map_err(|e| anyhow!("utf8: {e}"))?;
    Ok((s, &buf[pos + 1..]))
}

fn parse_parse_payload(payload: &[u8]) -> Result<(String, String, usize, Vec<u32>)> {
    let (name, rest) = read_cstring(payload)?;
    let (query, rest) = read_cstring(rest)?;
    if rest.len() < 2 {
        return Err(anyhow!("parse message too short for num_params"));
    }
    let num_params = i16::from_be_bytes(rest[0..2].try_into().map_err(|_| anyhow!("num_params"))?);
    if num_params < 0 {
        return Err(anyhow!("parse: invalid num_params {}", num_params));
    }
    let num_params = num_params as usize;
    let rest = &rest[2..];
    let need = num_params * 4;
    if rest.len() < need {
        return Err(anyhow!("parse: truncated parameter type OIDs"));
    }
    if rest.len() > need {
        return Err(anyhow!("parse: trailing bytes after parameter type OIDs"));
    }
    let mut param_oids = Vec::with_capacity(num_params);
    for i in 0..num_params {
        let off = i * 4;
        let oid = i32::from_be_bytes(
            rest[off..off + 4]
                .try_into()
                .map_err(|_| anyhow!("parse: invalid parameter type OID"))?,
        );
        if oid < 0 {
            return Err(anyhow!(
                "parse: invalid negative parameter type OID {}",
                oid
            ));
        }
        param_oids.push(oid as u32);
    }
    Ok((name.to_string(), query.to_string(), num_params, param_oids))
}

/// `parse_bind_payload` 的返回负载：portal、statement、参数值、参数格式、结果格式。
/// EN: Payload returned by `parse_bind_payload`: portal, statement, values, param formats, result formats.
type BindPayload = (String, String, Vec<BindParamValue>, Vec<i16>, Vec<i16>);

fn parse_bind_payload(payload: &[u8]) -> Result<BindPayload> {
    let (portal, rest) = read_cstring(payload)?;
    let (statement, mut rest) = read_cstring(rest)?;
    if rest.len() < 2 {
        return Err(anyhow!("bind message too short"));
    }
    let n_fmt = i16::from_be_bytes(rest[0..2].try_into().map_err(|_| anyhow!("n_fmt"))?);
    rest = &rest[2..];
    let mut param_formats: Vec<i16> = Vec::new();
    if n_fmt > 0 {
        let need = (n_fmt as usize) * 2;
        if rest.len() < need {
            return Err(anyhow!("bind: truncated format codes"));
        }
        for i in 0..(n_fmt as usize) {
            let off = i * 2;
            let f = i16::from_be_bytes(
                rest[off..off + 2]
                    .try_into()
                    .map_err(|_| anyhow!("bind: format code"))?,
            );
            param_formats.push(f);
        }
        rest = &rest[need..];
    } else if n_fmt < 0 {
        return Err(anyhow!("bind: invalid n_fmt"));
    }
    if rest.len() < 2 {
        return Err(anyhow!("bind: missing num_params"));
    }
    let num_params = i16::from_be_bytes(rest[0..2].try_into().map_err(|_| anyhow!("num_params"))?);
    rest = &rest[2..];
    if num_params < 0 {
        return Err(anyhow!("bind: invalid num_params"));
    }
    let mut params = Vec::new();
    let mut normalized_param_formats = Vec::with_capacity(num_params as usize);
    for i in 0..(num_params as usize) {
        if rest.len() < 4 {
            return Err(anyhow!("bind: missing parameter length"));
        }
        let plen = i32::from_be_bytes(rest[0..4].try_into().map_err(|_| anyhow!("param length"))?);
        rest = &rest[4..];
        if plen < 0 {
            return Err(anyhow!("bind: null parameter is not supported"));
        }
        let plen_usize = plen as usize;
        if rest.len() < plen_usize {
            return Err(anyhow!("bind: truncated parameter value"));
        }
        // 参数格式由 Bind 指定：0=text, 1=binary。当前其余格式仍显式拒绝。
        // EN: Parameter format is set by Bind: 0=text, 1=binary. Other formats are still rejected.
        let fmt = if n_fmt == 0 {
            0_i16
        } else if n_fmt == 1 {
            *param_formats.first().unwrap_or(&0_i16)
        } else {
            *param_formats.get(i).unwrap_or(&0_i16)
        };
        if fmt != 0 && fmt != 1 {
            return Err(anyhow!(
                "bind: unsupported parameter format code {} (only text/binary are supported)",
                fmt
            ));
        }
        normalized_param_formats.push(fmt);
        if fmt == 0 {
            let s = std::str::from_utf8(&rest[..plen_usize]).map_err(|e| anyhow!("utf8: {e}"))?;
            params.push(BindParamValue::Text(s.to_string()));
        } else {
            params.push(BindParamValue::Binary(rest[..plen_usize].to_vec()));
        }
        rest = &rest[plen_usize..];
    }
    if rest.len() < 2 {
        return Err(anyhow!("bind: missing result format codes"));
    }
    let r_fmt = i16::from_be_bytes(rest[0..2].try_into().map_err(|_| anyhow!("r_fmt"))?);
    rest = &rest[2..];
    let mut result_formats = Vec::new();
    if r_fmt > 0 {
        let need = (r_fmt as usize) * 2;
        if rest.len() < need {
            return Err(anyhow!("bind: truncated result format codes"));
        }
        for i in 0..(r_fmt as usize) {
            let off = i * 2;
            let f = i16::from_be_bytes(
                rest[off..off + 2]
                    .try_into()
                    .map_err(|_| anyhow!("bind: result format code"))?,
            );
            if f != 0 && f != 1 {
                return Err(anyhow!(
                    "bind: unsupported result format code {} (only text/binary are supported)",
                    f
                ));
            }
            result_formats.push(f);
        }
        rest = &rest[need..];
    } else if r_fmt < 0 {
        return Err(anyhow!("bind: invalid r_fmt"));
    }
    if !rest.is_empty() {
        return Err(anyhow!("bind: trailing bytes"));
    }
    Ok((
        portal.to_string(),
        statement.to_string(),
        params,
        normalized_param_formats,
        result_formats,
    ))
}

fn parse_execute_payload(payload: &[u8]) -> Result<(String, u32)> {
    let (portal, rest) = read_cstring(payload)?;
    if rest.len() < 4 {
        return Err(anyhow!("execute message too short"));
    }
    let max_rows = i32::from_be_bytes(rest[0..4].try_into().map_err(|_| anyhow!("max_rows"))?);
    if max_rows < 0 && max_rows != -1 {
        return Err(anyhow!("execute: invalid max_rows"));
    }
    let mr = if max_rows < 0 {
        u32::MAX
    } else {
        max_rows as u32
    };
    Ok((portal.to_string(), mr))
}

fn parse_describe_or_close_payload(payload: &[u8]) -> Result<(u8, String)> {
    if payload.is_empty() {
        return Err(anyhow!("describe/close: empty payload"));
    }
    let kind = payload[0];
    let (name, rest) = read_cstring(&payload[1..])?;
    if !rest.is_empty() {
        return Err(anyhow!("describe/close: trailing bytes"));
    }
    Ok((kind, name.to_string()))
}

fn parse_copy_fail_payload(payload: &[u8]) -> Result<String> {
    if payload.is_empty() {
        return Ok(String::new());
    }
    // 协议上通常是以 \0 终止的错误文本；此处宽容处理：若缺失终止符，则按整段 UTF-8 解析。
    // EN: Error text is normally \0-terminated; this is lenient: without a terminator, parse all as UTF-8.
    let raw = if *payload.last().unwrap_or(&0) == 0 {
        &payload[..payload.len().saturating_sub(1)]
    } else {
        payload
    };
    let s = String::from_utf8(raw.to_vec()).map_err(|e| anyhow!("copy fail utf8: {e}"))?;
    Ok(s)
}

pub fn decode_frontend_message(stream_buf: &[u8]) -> Result<FrontendMessage> {
    if stream_buf.len() < 5 {
        return Err(anyhow!("frontend message too short"));
    }
    let tag = stream_buf[0];
    // 长度字段来自网络：`i32 as usize` 会把负值（最高位 1，如 0xFFFFFFFF）符号
    // 扩展成巨大 usize，随后的 `len + 1` 在 debug 构建下加法溢出 panic（P4.5.1
    // fuzz 报出，输入 `[.., 0xff,0xff,0xff,0xff, ..]`）；release 只是靠回绕碰巧
    // 得到正确的 Err——「靠回绕凑对」不是可以依赖的行为。故先按 i32 语义拒绝负
    // 长度，再进 usize 域比较：此时 len ≤ i32::MAX，`len + 1` 不可能溢出。
    // EN: The length field comes off the wire. `i32 as usize` sign-extends a
    // negative value into a huge usize, making the following `len + 1`
    // overflow-panic in debug builds (found by fuzzing, P4.5.1); release only
    // landed on the right `Err` via wraparound, which is not a behavior worth
    // relying on. Reject negative lengths first, then compare in usize where
    // `len + 1` cannot overflow.
    let len = i32::from_be_bytes(
        stream_buf[1..5]
            .try_into()
            .map_err(|_| anyhow!("bad frontend length"))?,
    );
    let Ok(len) = usize::try_from(len) else {
        return Err(anyhow!("negative frontend message length"));
    };
    if len + 1 != stream_buf.len() {
        return Err(anyhow!("frontend message length mismatch"));
    }
    let payload = &stream_buf[5..];
    match tag {
        b'Q' => {
            if payload.is_empty() || *payload.last().unwrap_or(&1) != 0 {
                return Err(anyhow!("query payload must be null-terminated"));
            }
            let sql = String::from_utf8(payload[..payload.len() - 1].to_vec())?;
            Ok(FrontendMessage::Query(sql))
        }
        b'P' => {
            let (name, query, num_params, param_oids) = parse_parse_payload(payload)?;
            Ok(FrontendMessage::Parse {
                name,
                query,
                num_params,
                param_oids,
            })
        }
        b'B' => {
            let (portal, statement, params, param_formats, result_formats) =
                parse_bind_payload(payload)?;
            Ok(FrontendMessage::Bind {
                portal,
                statement,
                params,
                param_formats,
                result_formats,
            })
        }
        b'E' => {
            let (portal, max_rows) = parse_execute_payload(payload)?;
            Ok(FrontendMessage::Execute { portal, max_rows })
        }
        b'D' => {
            let (kind, name) = parse_describe_or_close_payload(payload)?;
            Ok(FrontendMessage::Describe { kind, name })
        }
        b'C' => {
            let (kind, name) = parse_describe_or_close_payload(payload)?;
            Ok(FrontendMessage::Close { kind, name })
        }
        b'S' => {
            if !payload.is_empty() {
                return Err(anyhow!("sync must have empty payload"));
            }
            Ok(FrontendMessage::Sync)
        }
        b'H' => {
            if !payload.is_empty() {
                return Err(anyhow!("flush must have empty payload"));
            }
            Ok(FrontendMessage::Flush)
        }
        b'd' => Ok(FrontendMessage::CopyData),
        b'c' => Ok(FrontendMessage::CopyDone),
        b'f' => Ok(FrontendMessage::CopyFail(parse_copy_fail_payload(payload)?)),
        b'X' => Ok(FrontendMessage::Terminate),
        other => Ok(FrontendMessage::Unsupported(other)),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        decode_frontend_message, decode_startup_packet, encode_authentication_ok,
        encode_command_complete, encode_data_row_single_text, encode_error_response,
        encode_notice_response, encode_ready_for_query, encode_row_description_single_text,
        BindParamValue, FrontendMessage, PROTOCOL_VERSION_3,
    };

    #[test]
    fn decode_startup_packet_parses_protocol_and_params() {
        let mut payload = Vec::new();
        payload.extend_from_slice(&PROTOCOL_VERSION_3.to_be_bytes());
        payload.extend_from_slice(b"user\0alice\0database\0houyi\0\0");
        let len = (payload.len() + 4) as i32;

        let mut packet = Vec::new();
        packet.extend_from_slice(&len.to_be_bytes());
        packet.extend_from_slice(&payload);

        let startup = decode_startup_packet(&packet).expect("decode should pass");
        assert_eq!(startup.protocol_version, PROTOCOL_VERSION_3);
        assert_eq!(startup.params.get("user"), Some(&"alice".to_string()));
        assert_eq!(startup.params.get("database"), Some(&"houyi".to_string()));
    }

    #[test]
    fn encode_core_messages_have_expected_tags() {
        let auth = encode_authentication_ok();
        assert_eq!(auth[0], b'R');
        let ready = encode_ready_for_query();
        assert_eq!(ready[0], b'Z');
        let complete = encode_command_complete("SELECT 1");
        assert_eq!(complete[0], b'C');
        let row_desc = encode_row_description_single_text("value");
        assert_eq!(row_desc[0], b'T');
        let data_row = encode_data_row_single_text("1");
        assert_eq!(data_row[0], b'D');
        let err = encode_error_response("ERROR", "0A000", "unsupported");
        assert_eq!(err[0], b'E');
        let notice = encode_notice_response("NOTICE", "placeholder");
        assert_eq!(notice[0], b'N');
    }

    #[test]
    fn decode_simple_query_message() {
        let sql = b"SELECT 1\0";
        let len = (sql.len() + 4) as i32;
        let mut msg = Vec::new();
        msg.push(b'Q');
        msg.extend_from_slice(&len.to_be_bytes());
        msg.extend_from_slice(sql);
        let decoded = decode_frontend_message(&msg).expect("decode query");
        assert_eq!(decoded, FrontendMessage::Query("SELECT 1".to_string()));
    }

    #[test]
    fn decode_extended_parse_bind_execute_sync() {
        let mut p = Vec::new();
        p.push(b'P');
        let parse_body = b"\0SELECT 1\0\x00\x00";
        p.extend_from_slice(&((4 + parse_body.len()) as i32).to_be_bytes());
        p.extend_from_slice(parse_body);
        assert_eq!(
            decode_frontend_message(&p).unwrap(),
            FrontendMessage::Parse {
                name: String::new(),
                query: "SELECT 1".to_string(),
                num_params: 0,
                param_oids: Vec::new(),
            }
        );

        let mut b = Vec::new();
        b.push(b'B');
        let bind_body = b"\0\0\0\0\0\0\0\0";
        b.extend_from_slice(&((4 + bind_body.len()) as i32).to_be_bytes());
        b.extend_from_slice(bind_body);
        assert_eq!(
            decode_frontend_message(&b).unwrap(),
            FrontendMessage::Bind {
                portal: String::new(),
                statement: String::new(),
                params: Vec::new(),
                param_formats: Vec::new(),
                result_formats: Vec::new(),
            }
        );

        let mut e = Vec::new();
        e.push(b'E');
        let mut ex = Vec::new();
        ex.push(0);
        ex.extend_from_slice(&(-1_i32).to_be_bytes());
        e.extend_from_slice(&((4 + ex.len()) as i32).to_be_bytes());
        e.extend_from_slice(&ex);
        assert_eq!(
            decode_frontend_message(&e).unwrap(),
            FrontendMessage::Execute {
                portal: String::new(),
                max_rows: u32::MAX,
            }
        );

        let mut s = Vec::new();
        s.push(b'S');
        s.extend_from_slice(&4_i32.to_be_bytes());
        assert_eq!(decode_frontend_message(&s).unwrap(), FrontendMessage::Sync);
    }

    #[test]
    fn decode_bind_with_one_text_parameter() {
        let mut body = Vec::new();
        body.extend_from_slice(b"\0"); // unnamed portal
        body.extend_from_slice(b"\0"); // unnamed statement
        body.extend_from_slice(&0_i16.to_be_bytes()); // no explicit param format codes -> text
        body.extend_from_slice(&1_i16.to_be_bytes()); // one parameter
        body.extend_from_slice(&2_i32.to_be_bytes()); // len
        body.extend_from_slice(b"42");
        body.extend_from_slice(&0_i16.to_be_bytes()); // result format count

        let mut msg = Vec::new();
        msg.push(b'B');
        msg.extend_from_slice(&((4 + body.len()) as i32).to_be_bytes());
        msg.extend_from_slice(&body);
        assert_eq!(
            decode_frontend_message(&msg).unwrap(),
            FrontendMessage::Bind {
                portal: String::new(),
                statement: String::new(),
                params: vec![BindParamValue::Text("42".to_string())],
                param_formats: vec![0],
                result_formats: Vec::new(),
            }
        );
    }

    #[test]
    fn decode_parse_with_two_params() {
        let mut p = Vec::new();
        p.push(b'P');
        let parse_body =
            b"\0SELECT id FROM docs WHERE id IN ($1,$2)\0\x00\x02\x00\x00\x00\x00\x00\x00\x00\x00";
        p.extend_from_slice(&((4 + parse_body.len()) as i32).to_be_bytes());
        p.extend_from_slice(parse_body);
        assert_eq!(
            decode_frontend_message(&p).unwrap(),
            FrontendMessage::Parse {
                name: String::new(),
                query: "SELECT id FROM docs WHERE id IN ($1,$2)".to_string(),
                num_params: 2,
                param_oids: vec![0, 0],
            }
        );
    }

    #[test]
    fn decode_parse_with_explicit_param_oids() {
        let mut p = Vec::new();
        p.push(b'P');
        let mut parse_body = Vec::new();
        parse_body.extend_from_slice(b"\0SELECT id FROM docs WHERE id = $1\0");
        parse_body.extend_from_slice(&1_i16.to_be_bytes());
        parse_body.extend_from_slice(&25_i32.to_be_bytes()); // TEXTOID
        p.extend_from_slice(&((4 + parse_body.len()) as i32).to_be_bytes());
        p.extend_from_slice(&parse_body);
        assert_eq!(
            decode_frontend_message(&p).unwrap(),
            FrontendMessage::Parse {
                name: String::new(),
                query: "SELECT id FROM docs WHERE id = $1".to_string(),
                num_params: 1,
                param_oids: vec![25],
            }
        );
    }

    #[test]
    fn decode_bind_with_two_text_parameters() {
        let mut body = Vec::new();
        body.extend_from_slice(b"\0"); // unnamed portal
        body.extend_from_slice(b"\0"); // unnamed statement
        body.extend_from_slice(&0_i16.to_be_bytes()); // no explicit param format codes -> text
        body.extend_from_slice(&2_i16.to_be_bytes()); // two parameters
        body.extend_from_slice(&1_i32.to_be_bytes());
        body.extend_from_slice(b"7");
        body.extend_from_slice(&1_i32.to_be_bytes());
        body.extend_from_slice(b"9");
        body.extend_from_slice(&0_i16.to_be_bytes()); // result format count

        let mut msg = Vec::new();
        msg.push(b'B');
        msg.extend_from_slice(&((4 + body.len()) as i32).to_be_bytes());
        msg.extend_from_slice(&body);
        assert_eq!(
            decode_frontend_message(&msg).unwrap(),
            FrontendMessage::Bind {
                portal: String::new(),
                statement: String::new(),
                params: vec![
                    BindParamValue::Text("7".to_string()),
                    BindParamValue::Text("9".to_string()),
                ],
                param_formats: vec![0, 0],
                result_formats: Vec::new(),
            }
        );
    }

    #[test]
    fn decode_bind_with_one_shared_text_format_code() {
        let mut body = Vec::new();
        body.extend_from_slice(b"\0");
        body.extend_from_slice(b"\0");
        body.extend_from_slice(&1_i16.to_be_bytes()); // one shared format code
        body.extend_from_slice(&0_i16.to_be_bytes()); // text
        body.extend_from_slice(&2_i16.to_be_bytes()); // two params
        body.extend_from_slice(&1_i32.to_be_bytes());
        body.extend_from_slice(b"7");
        body.extend_from_slice(&1_i32.to_be_bytes());
        body.extend_from_slice(b"9");
        body.extend_from_slice(&0_i16.to_be_bytes()); // result format count

        let mut msg = Vec::new();
        msg.push(b'B');
        msg.extend_from_slice(&((4 + body.len()) as i32).to_be_bytes());
        msg.extend_from_slice(&body);
        assert_eq!(
            decode_frontend_message(&msg).unwrap(),
            FrontendMessage::Bind {
                portal: String::new(),
                statement: String::new(),
                params: vec![
                    BindParamValue::Text("7".to_string()),
                    BindParamValue::Text("9".to_string()),
                ],
                param_formats: vec![0, 0],
                result_formats: Vec::new(),
            }
        );
    }

    #[test]
    fn decode_bind_with_one_binary_int4_parameter() {
        let mut body = Vec::new();
        body.extend_from_slice(b"\0");
        body.extend_from_slice(b"\0");
        body.extend_from_slice(&1_i16.to_be_bytes()); // one shared format code
        body.extend_from_slice(&1_i16.to_be_bytes()); // binary
        body.extend_from_slice(&1_i16.to_be_bytes()); // one param
        body.extend_from_slice(&4_i32.to_be_bytes()); // len
        body.extend_from_slice(&7_i32.to_be_bytes());
        body.extend_from_slice(&0_i16.to_be_bytes()); // result format count

        let mut msg = Vec::new();
        msg.push(b'B');
        msg.extend_from_slice(&((4 + body.len()) as i32).to_be_bytes());
        msg.extend_from_slice(&body);
        assert_eq!(
            decode_frontend_message(&msg).unwrap(),
            FrontendMessage::Bind {
                portal: String::new(),
                statement: String::new(),
                params: vec![BindParamValue::Binary(7_i32.to_be_bytes().to_vec())],
                param_formats: vec![1],
                result_formats: Vec::new(),
            }
        );
    }

    #[test]
    fn decode_bind_with_binary_result_format() {
        let mut body = Vec::new();
        body.extend_from_slice(b"\0"); // unnamed portal
        body.extend_from_slice(b"\0"); // unnamed statement
        body.extend_from_slice(&0_i16.to_be_bytes()); // no explicit param format codes -> text
        body.extend_from_slice(&0_i16.to_be_bytes()); // zero parameters
        body.extend_from_slice(&1_i16.to_be_bytes()); // one result format code follows
        body.extend_from_slice(&1_i16.to_be_bytes()); // binary result format (unsupported)

        let mut msg = Vec::new();
        msg.push(b'B');
        msg.extend_from_slice(&((4 + body.len()) as i32).to_be_bytes());
        msg.extend_from_slice(&body);

        assert_eq!(
            decode_frontend_message(&msg).unwrap(),
            FrontendMessage::Bind {
                portal: String::new(),
                statement: String::new(),
                params: Vec::new(),
                param_formats: Vec::new(),
                result_formats: vec![1],
            }
        );
    }

    #[test]
    fn decode_copy_messages() {
        let mut d = Vec::new();
        d.push(b'd');
        d.extend_from_slice(&4_i32.to_be_bytes());
        assert_eq!(
            decode_frontend_message(&d).unwrap(),
            FrontendMessage::CopyData
        );

        let mut c = Vec::new();
        c.push(b'c');
        c.extend_from_slice(&4_i32.to_be_bytes());
        assert_eq!(
            decode_frontend_message(&c).unwrap(),
            FrontendMessage::CopyDone
        );

        let mut f = Vec::new();
        f.push(b'f');
        let f_body = b"client copy failed\0";
        f.extend_from_slice(&((4 + f_body.len()) as i32).to_be_bytes());
        f.extend_from_slice(f_body);
        assert_eq!(
            decode_frontend_message(&f).unwrap(),
            FrontendMessage::CopyFail("client copy failed".to_string())
        );
    }

    /// 回归（P4.5.1 fuzz 崩溃样本 crash-deee14f2…）：长度字段为负必须干净 `Err`。
    /// 旧实现 `i32 as usize` 把 -1 符号扩展成 `usize::MAX`，随后 `len + 1` 在
    /// debug 构建下加法溢出 panic（release 仅靠回绕碰巧得到正确的 Err）。
    /// EN: Regression (P4.5.1 fuzz crash sample crash-deee14f2...): negative length must be a clean `Err`.
    /// EN: The old `i32 as usize` sign-extended -1 to `usize::MAX`, then `len + 1` overflowed
    /// EN: and panicked in debug (release only got the right Err by wrapping by accident).
    #[test]
    fn frontend_message_negative_length_is_rejected() {
        for bad_len in [-1_i32, i32::MIN] {
            let mut buf = vec![b'Q'];
            buf.extend_from_slice(&bad_len.to_be_bytes());
            buf.push(0); // 载荷 1 字节，凑够 5 字节最小长度
            let err = decode_frontend_message(&buf)
                .expect_err("negative length must be rejected, not overflow");
            assert!(
                err.to_string().contains("negative"),
                "len={bad_len}: unexpected error: {err}"
            );
        }
    }
}
