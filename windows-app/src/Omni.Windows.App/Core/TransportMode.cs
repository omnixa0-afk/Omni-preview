namespace Omni.Windows.App.Core;

internal enum TransportMode
{
    Quic,
}

internal static class TransportModeExtensions
{
    public static string ToDbValue(this TransportMode mode)
        => mode switch
        {
            TransportMode.Quic => "quic",
            _ => "quic",
        };

    public static string ToDisplayName(this TransportMode mode)
        => mode switch
        {
            TransportMode.Quic => "QUIC",
            _ => "QUIC",
        };

    public static string ToDescription(this TransportMode mode)
        => mode switch
        {
            TransportMode.Quic =>
                "Современный UDP-транспорт с TLS 1.3 внутри QUIC. Удобен для сообщений, файлов и адресов подключения.",
            _ => TransportMode.Quic.ToDescription(),
        };

    public static bool TryFromDbValue(string? value, out TransportMode mode)
    {
        switch (value?.Trim().ToLowerInvariant())
        {
            case "quic":
                mode = TransportMode.Quic;
                return true;
            default:
                mode = TransportMode.Quic;
                return false;
        }
    }

    public static TransportMode FromDbValue(string? value)
        => TryFromDbValue(value, out TransportMode mode)
            ? mode
            : TransportMode.Quic;
}
