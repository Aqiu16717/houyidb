package com.houyidb.client;

/**
 * 客户端错误：HTTP 非 2xx（{@link #statusCode()} + 服务端纯文本诊断，
 * axum 元组错误原样透出）。传输层失败仍以 {@link java.io.IOException} 抛出。
 * EN: Client error: HTTP non-2xx ({@link #statusCode()} plus the server's
 * plain-text diagnostic, axum tuple errors passed through verbatim). Transport
 * failures are still thrown as {@link java.io.IOException}.
 */
public final class HouyiException extends Exception {

    private final int statusCode;
    private final String serverMessage;

    public HouyiException(int statusCode, String serverMessage) {
        super("houyidb HTTP " + statusCode + ": " + serverMessage);
        this.statusCode = statusCode;
        this.serverMessage = serverMessage;
    }

    /**
     * 服务端返回的 HTTP 状态码。
     * EN: HTTP status code returned by the server.
     */
    public int statusCode() {
        return statusCode;
    }

    /**
     * 服务端返回的纯文本诊断。
     * EN: Plain-text diagnostic returned by the server.
     */
    public String serverMessage() {
        return serverMessage;
    }
}
