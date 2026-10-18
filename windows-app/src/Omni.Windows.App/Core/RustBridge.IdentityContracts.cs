using System.Collections.Generic;
using System.Text.Json.Serialization;

namespace Omni.Windows.App.Core;

// Identity / verification wire contracts (account + device descriptors, the X3DH prekey
// bundle, and the contact-verification / safety-number payloads). These back the
// contact-exchange and verification flows — NOT signaling. (Renamed 2026-06-04 from
// RustBridge.SignalingContracts.cs, which no longer described the contents.)

internal sealed record AccountPublicDescriptorV1(
    [property: JsonPropertyName("version")] ushort Version,
    [property: JsonPropertyName("account_id")] string AccountId,
    [property: JsonPropertyName("display_name")] string DisplayName);

internal sealed record DevicePublicDescriptorV1(
    [property: JsonPropertyName("version")] ushort Version,
    [property: JsonPropertyName("device_id")] string DeviceId,
    [property: JsonPropertyName("account_id")] string AccountId,
    [property: JsonPropertyName("device_label")] string DeviceLabel);

internal sealed record AccountCryptoDescriptorV1(
    [property: JsonPropertyName("version")] ushort Version,
    [property: JsonPropertyName("account_id")] string AccountId,
    [property: JsonPropertyName("identity_public_key")] string IdentityPublicKey,
    [property: JsonPropertyName("identity_signing_public_key")] string IdentitySigningPublicKey);

internal sealed record DeviceCryptoDescriptorV1(
    [property: JsonPropertyName("version")] ushort Version,
    [property: JsonPropertyName("device_id")] string DeviceId,
    [property: JsonPropertyName("account_id")] string AccountId,
    [property: JsonPropertyName("device_label")] string DeviceLabel,
    [property: JsonPropertyName("device_public_key")] string DevicePublicKey);

internal sealed record SignedPrekeyDescriptorV1(
    [property: JsonPropertyName("prekey_id")] uint PrekeyId,
    [property: JsonPropertyName("public_key")] string PublicKey,
    [property: JsonPropertyName("signature_r")] string SignatureR,
    [property: JsonPropertyName("signature_s")] string SignatureS,
    [property: JsonPropertyName("expires_at_unix_ms")] long? ExpiresAtUnixMs);

internal sealed record OneTimePrekeyDescriptorV1(
    [property: JsonPropertyName("prekey_id")] uint PrekeyId,
    [property: JsonPropertyName("public_key")] string PublicKey);

internal sealed record PrekeyBundleV1(
    [property: JsonPropertyName("version")] ushort Version,
    [property: JsonPropertyName("account")] AccountCryptoDescriptorV1 Account,
    [property: JsonPropertyName("device")] DeviceCryptoDescriptorV1 Device,
    [property: JsonPropertyName("signed_prekey")] SignedPrekeyDescriptorV1 SignedPrekey,
    [property: JsonPropertyName("one_time_prekey")] OneTimePrekeyDescriptorV1? OneTimePrekey,
    [property: JsonPropertyName("generated_at_unix_ms")] long GeneratedAtUnixMs);

internal sealed record ContactVerificationPayloadV1(
    [property: JsonPropertyName("version")] ushort Version,
    [property: JsonPropertyName("account")] AccountPublicDescriptorV1 Account,
    [property: JsonPropertyName("devices")] IReadOnlyList<DevicePublicDescriptorV1> Devices,
    [property: JsonPropertyName("fingerprint")] string Fingerprint,
    [property: JsonPropertyName("safety_number")] string SafetyNumber,
    [property: JsonPropertyName("exported_at_unix_ms")] long ExportedAtUnixMs);

internal sealed record MyVerificationPayloadV1(
    [property: JsonPropertyName("version")] ushort Version,
    [property: JsonPropertyName("account")] AccountPublicDescriptorV1 Account,
    [property: JsonPropertyName("devices")] IReadOnlyList<DevicePublicDescriptorV1> Devices,
    [property: JsonPropertyName("fingerprint")] string Fingerprint,
    [property: JsonPropertyName("safety_number")] string SafetyNumber,
    [property: JsonPropertyName("exported_at_unix_ms")] long ExportedAtUnixMs);

internal sealed record ImportableContactPayloadV1(
    [property: JsonPropertyName("version")] ushort Version,
    [property: JsonPropertyName("account")] AccountPublicDescriptorV1 Account,
    [property: JsonPropertyName("devices")] IReadOnlyList<DevicePublicDescriptorV1> Devices,
    [property: JsonPropertyName("fingerprint")] string Fingerprint,
    [property: JsonPropertyName("safety_number")] string SafetyNumber,
    [property: JsonPropertyName("exported_at_unix_ms")] long ExportedAtUnixMs);
