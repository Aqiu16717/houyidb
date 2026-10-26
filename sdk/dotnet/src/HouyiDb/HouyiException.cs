namespace HouyiDb;

/// <summary>
/// 客户端错误：HTTP 非 2xx（<see cref="StatusCode"/> + 服务端纯文本诊断，
/// axum 元组错误原样透出）。传输层失败仍以 <see cref="HttpRequestException"/> 抛出。
/// EN: Client error: HTTP non-2xx (<see cref="StatusCode"/> plus the server's
/// plain-text diagnostic, axum tuple errors passed through verbatim). Transport
/// failures are still thrown as <see cref="HttpRequestException"/>.
/// </summary>
public sealed class HouyiException : Exception
{
    public HouyiException(int statusCode, string serverMessage)
        : base($"houyidb HTTP {statusCode}: {serverMessage}")
    {
        StatusCode = statusCode;
        ServerMessage = serverMessage;
    }

    /// <summary>
    /// 服务端返回的 HTTP 状态码。
    /// EN: HTTP status code returned by the server.
    /// </summary>
    public int StatusCode { get; }

    /// <summary>
    /// 服务端返回的纯文本诊断。
    /// EN: Plain-text diagnostic returned by the server.
    /// </summary>
    public string ServerMessage { get; }
}
