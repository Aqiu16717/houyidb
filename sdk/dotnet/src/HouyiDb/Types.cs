using System.Text.Json;
using System.Text.Json.Serialization;

namespace HouyiDb;

/// <summary>
/// 一条待写入向量：id + 分量 + 可选 label/payload。
/// EN: A vector to write: id + components + optional label/payload.
/// </summary>
public sealed class Vector
{
    [JsonPropertyName("id")]
    public ulong Id { get; set; }

    [JsonPropertyName("values")]
    public required float[] Values { get; set; }

    [JsonPropertyName("label"), JsonIgnore(Condition = JsonIgnoreCondition.WhenWritingNull)]
    public string? Label { get; set; }

    [JsonPropertyName("payload"), JsonIgnore(Condition = JsonIgnoreCondition.WhenWritingNull)]
    public Dictionary<string, object?>? Payload { get; set; }
}

/// <summary>
/// 过滤算子白名单（与服务端一致；多条 <see cref="Filter"/> 之间取交集）。
/// wire 值即小写字符串，直接序列化，无需枚举转换器。
/// EN: Filter operator whitelist (matches the server; multiple <see cref="Filter"/>
/// entries are intersected). The wire value is the lowercase string, serialized
/// directly with no enum converter.
/// </summary>
public static class FilterOp
{
    public const string Eq = "eq";
    public const string Gt = "gt";
    public const string Gte = "gte";
    public const string Lt = "lt";
    public const string Lte = "lte";
    public const string In = "in";
}

/// <summary>
/// 一条结构化过滤条件。
/// EN: One structured filter condition.
/// </summary>
public sealed class Filter
{
    public Filter(string column, string op, object value)
    {
        Column = column;
        Op = op;
        Value = value;
    }

    [JsonPropertyName("column")]
    public string Column { get; }

    [JsonPropertyName("op")]
    public string Op { get; }

    [JsonPropertyName("value")]
    public object Value { get; }
}

/// <summary>
/// 单条命中：id + 相似度分数 + 可选 payload（include_payload 时）。
/// EN: One hit: id + similarity score + optional payload (when include_payload is set).
/// </summary>
public sealed class SearchHit
{
    public required ulong Id { get; init; }
    public required double Score { get; init; }
    public IReadOnlyDictionary<string, JsonElement>? Payload { get; init; }
}

/// <summary>
/// 检索响应：hits 按相似度从高到低排列。
/// EN: Search response: hits ordered from highest to lowest similarity.
/// </summary>
public sealed class SearchResult
{
    public required IReadOnlyList<SearchHit> Hits { get; init; }

    public ulong[] Ids() => Hits.Select(h => h.Id).ToArray();

    public double[] Scores() => Hits.Select(h => h.Score).ToArray();

    /// <summary>
    /// 检索响应的 wire 形状：ids/scores 平行数组 + payloads 以 id 十进制字符串为键。
    /// EN: Wire shape of the search response: parallel ids/scores arrays plus
    /// payloads keyed by the decimal id string.
    /// </summary>
    internal sealed class Wire
    {
        [JsonPropertyName("ids")]
        public ulong[] Ids { get; set; } = [];

        [JsonPropertyName("scores")]
        public double[] Scores { get; set; } = [];

        [JsonPropertyName("payloads")]
        public Dictionary<string, Dictionary<string, JsonElement>> Payloads { get; set; } = new();
    }

    internal static SearchResult FromWire(Wire wire) => new()
    {
        Hits = wire.Ids.Select((id, i) => new SearchHit
        {
            Id = id,
            Score = i < wire.Scores.Length ? wire.Scores[i] : 0.0,
            Payload = wire.Payloads.TryGetValue(id.ToString(), out var p) ? p : null,
        }).ToList(),
    };
}

/// <summary>
/// 检索/混合检索可选参数（默认构造即全默认）。
/// EN: Optional search/hybrid-search parameters (default construction means all defaults).
/// </summary>
public sealed class SearchOptions
{
    public int? EfSearch { get; set; }
    public bool IncludePayload { get; set; }
    public List<Filter> Filters { get; } = [];
}
