using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Omni.Windows.App.Core;
using Omni.Windows.App.Features.Calls;
using Omni.Windows.App.Presentation;
using Omni.Windows.App.ViewModels;
using System.Text.Json.Nodes;

namespace Omni.Windows.App;

public sealed partial class MainWindow
{
    private sealed record CallVideoCodecPreferenceOption(
        CallVideoCodecPreference Preference,
        string DisplayName);

    private sealed record CallVideoBitratePreferenceOption(
        CallVideoBitratePreference Preference,
        string DisplayName);

    private sealed record CallVideoFrameRatePreferenceOption(
        CallVideoFrameRatePreference Preference,
        string DisplayName);

    private void InitializeCallVideoPreferencePickers()
    {
        CallVideoFrameRateComboBox.DisplayMemberPath = nameof(CallVideoFrameRatePreferenceOption.DisplayName);
        CallVideoFrameRateComboBox.ItemsSource = callVideoFrameRatePreferenceOptions;
        CallVideoCodecComboBox.DisplayMemberPath = nameof(CallVideoCodecPreferenceOption.DisplayName);
        CallVideoCodecComboBox.ItemsSource = callVideoCodecPreferenceOptions;
        CallVideoBitrateComboBox.DisplayMemberPath = nameof(CallVideoBitratePreferenceOption.DisplayName);
        CallVideoBitrateComboBox.ItemsSource = callVideoBitratePreferenceOptions;
        // Before any call: the stored level goes straight into the service, nothing to renegotiate.
        _ = callVideoService.ApplyBitratePreferenceAsync(LoadPersistedCallVideoBitrate());
        RefreshCallVideoPreferenceSelection();
    }

    // Settings → Calls → «Битрейт»: per device, device-preferences.json key "callVideoBitrate".
    private CallVideoBitratePreference LoadPersistedCallVideoBitrate()
    {
        try
        {
            return CallVideoPreferences.ParseBitratePreference(
                ReadDevicePreferencesDocument()["callVideoBitrate"]?.GetValue<string>());
        }
        catch (Exception ex)
        {
            // A spoiled value falls back to Auto rather than locking the user into a level they did not pick.
            AppTrace.WriteFailure("calls/bitrate", "load", ex);
            return CallVideoBitratePreference.Auto;
        }
    }

    private async void OnCallVideoBitrateSelectionChanged(object sender, SelectionChangedEventArgs e)
    {
        if (suppressCallVideoPreferenceSelection ||
            CallVideoBitrateComboBox.SelectedItem is not CallVideoBitratePreferenceOption option)
        {
            return;
        }

        try
        {
            JsonObject document = ReadDevicePreferencesDocument();
            document["callVideoBitrate"] = CallVideoPreferences.FormatBitratePreference(option.Preference);
            WriteDevicePreferencesDocument(document);
            await callVideoService.ApplyBitratePreferenceAsync(option.Preference);
            SetActionStatus($"Битрейт видео: {option.DisplayName}.");
            RenderCurrentChatCallSummary();
        }
        catch (Exception ex)
        {
            RefreshCallVideoPreferenceSelection();
            SetActionStatus($"Не удалось применить битрейт видео: {NormalizeUserError(ex)}");
        }
    }

    private void RefreshCallVideoPreferenceSelection()
    {
        CallVideoPreferenceSnapshot preferences = callVideoService.GetPreferences();
        CallVideoFrameRatePreferenceOption frameRateOption = callVideoFrameRatePreferenceOptions
            .FirstOrDefault(option => option.Preference == preferences.FrameRate)
            ?? callVideoFrameRatePreferenceOptions[0];
        CallVideoCodecPreferenceOption codecOption = callVideoCodecPreferenceOptions
            .FirstOrDefault(option => option.Preference == preferences.Codec)
            ?? callVideoCodecPreferenceOptions[0];
        CallVideoBitratePreferenceOption bitrateOption = callVideoBitratePreferenceOptions
            .FirstOrDefault(option => option.Preference == callVideoService.BitratePreference)
            ?? callVideoBitratePreferenceOptions[1];

        suppressCallVideoPreferenceSelection = true;
        try
        {
            CallVideoFrameRateComboBox.SelectedItem = frameRateOption;
            CallVideoCodecComboBox.SelectedItem = codecOption;
            CallVideoBitrateComboBox.SelectedItem = bitrateOption;
        }
        finally
        {
            suppressCallVideoPreferenceSelection = false;
        }
    }

    // --- Camera picker (Settings → "Камера"): per-window choice, persisted in the app-dir's
    // device-preferences.json. The chosen MediaFrameSourceGroup.Id is pushed to CallVideoService,
    // which prefers it when opening the webcam. Each window is its own process → inherently per-window.

    private sealed record CameraPickerOption(string? DeviceId, string DisplayName);

    private bool suppressCameraSelection;

    private bool cameraPickerPopulated;

    // Cheap, runs at startup: push the persisted preferred-camera id to the call service so the
    // next call opens the right webcam. Does NOT enumerate cameras — that loads the Media
    // Foundation sensor stack (MFPlat/MFSENSORGROUP) and is only needed to fill the Settings combo.
    private void InitializeCameraPicker()
    {
        SettingsCameraComboBox.DisplayMemberPath = nameof(CameraPickerOption.DisplayName);
        callVideoService.PreferredCameraId = LoadPreferredCameraId();
    }

    // Lazy, runs on first Settings view: enumerate cameras and fill the combo. Deferred off the
    // boot path so camera enumeration (and its MF sensor stack) only happens when the user opens
    // Settings. Guarded so it runs once; a failure leaves the guard clear for a retry.
    private void EnsureCameraPickerPopulated()
    {
        if (cameraPickerPopulated)
        {
            return;
        }

        cameraPickerPopulated = true;
        _ = PopulateCameraPickerAsync();
    }

    private async Task PopulateCameraPickerAsync()
    {
        try
        {
            string? persistedId = LoadPreferredCameraId();
            callVideoService.PreferredCameraId = persistedId;

            IReadOnlyList<CallVideoService.CameraOption> cameras = await CallVideoService.ListCamerasAsync();

            var options = new List<CameraPickerOption> { new(null, "Системная по умолчанию") };
            foreach (CallVideoService.CameraOption camera in cameras)
            {
                options.Add(new CameraPickerOption(camera.Id, camera.DisplayName));
            }

            CameraPickerOption selected = options
                .FirstOrDefault(option => string.Equals(option.DeviceId, persistedId, StringComparison.OrdinalIgnoreCase))
                ?? options[0];

            suppressCameraSelection = true;
            try
            {
                SettingsCameraComboBox.ItemsSource = options;
                SettingsCameraComboBox.SelectedItem = selected;
            }
            finally
            {
                suppressCameraSelection = false;
            }

            // A previously-chosen camera that is no longer present falls back to the system default.
            if (selected.DeviceId is null && persistedId is not null)
            {
                callVideoService.PreferredCameraId = null;
                SavePreferredCameraId(null);
            }

            UpdateCameraPickerSummary(cameras.Count, selected);
        }
        catch (Exception ex)
        {
            cameraPickerPopulated = false;
            SetActionStatus($"Не удалось загрузить список камер: {NormalizeUserError(ex)}");
        }
    }

    private void OnSettingsCameraSelectionChanged(object sender, SelectionChangedEventArgs e)
    {
        if (suppressCameraSelection)
        {
            return;
        }

        if (SettingsCameraComboBox.SelectedItem is not CameraPickerOption option)
        {
            return;
        }

        callVideoService.PreferredCameraId = option.DeviceId;
        SavePreferredCameraId(option.DeviceId);

        int cameraCount = SettingsCameraComboBox.Items.Count - 1; // minus the "system default" sentinel
        UpdateCameraPickerSummary(cameraCount < 0 ? 0 : cameraCount, option);
        SetActionStatus(option.DeviceId is null
            ? "Камера: системная по умолчанию."
            : $"Камера: {option.DisplayName}.");
    }

    private void UpdateCameraPickerSummary(int cameraCount, CameraPickerOption selected)
    {
        SettingsCameraSummaryTextBlock.Text = cameraCount switch
        {
            0 => "Камеры не найдены. Звонок идёт только со звуком, пока камера не появится.",
            _ when selected.DeviceId is null => "Используется системная камера по умолчанию. Изменение применится при следующем включении камеры.",
            _ => $"Выбрано: {selected.DisplayName}. Изменение применится при следующем включении камеры.",
        };
    }

    // --- Microphone / speakers (Settings → «Звонки»): IMMDevice ids from the native media DLL,
    // persisted per window in device-preferences.json next to the camera; null = «Как в Windows».

    private sealed record AudioDevicePickerOption(string? DeviceId, string DisplayName);

    private bool suppressAudioDeviceSelection;

    // Cheap, runs at startup: push the persisted choice to the call service. No enumeration.
    private void InitializeAudioDevicePickers()
    {
        SettingsMicrophoneComboBox.DisplayMemberPath = nameof(AudioDevicePickerOption.DisplayName);
        SettingsSpeakerComboBox.DisplayMemberPath = nameof(AudioDevicePickerOption.DisplayName);
        callVideoService.PreferredMicrophoneId = LoadPreferredAudioDeviceId(MicrophonePreferenceKey);
        callVideoService.PreferredSpeakerId = LoadPreferredAudioDeviceId(SpeakerPreferenceKey);
    }

    private const string MicrophonePreferenceKey = "microphoneDeviceId";
    private const string SpeakerPreferenceKey = "speakerDeviceId";

    // On every Settings view (not once, like the camera): a headset plugged in while the window
    // was open must show up the next time Settings is opened. Enumeration is a few COM calls, off
    // the UI thread.
    private async void RefreshAudioDevicePickers()
    {
        try
        {
            NativeWebRtcMediaEngine.AudioDeviceList devices =
                await Task.Run(NativeWebRtcMediaEngine.ListAudioDevices);
            PopulateAudioDeviceCombo(SettingsMicrophoneComboBox, devices.Microphones, callVideoService.PreferredMicrophoneId);
            PopulateAudioDeviceCombo(SettingsSpeakerComboBox, devices.Speakers, callVideoService.PreferredSpeakerId);
            UpdateAudioDevicesSummary(devices);
        }
        catch (Exception ex)
        {
            SettingsAudioDevicesSummaryTextBlock.Text = $"Не удалось получить список устройств: {NormalizeUserError(ex)}";
        }
    }

    private void PopulateAudioDeviceCombo(
        ComboBox combo,
        IReadOnlyList<NativeWebRtcMediaEngine.AudioDeviceOption> devices,
        string? selectedId)
    {
        NativeWebRtcMediaEngine.AudioDeviceOption? windowsDefault = devices.FirstOrDefault(device => device.IsDefault);
        var options = new List<AudioDevicePickerOption>
        {
            new(null, windowsDefault is null ? "Как в Windows" : $"Как в Windows: {windowsDefault.Name}"),
        };
        options.AddRange(devices.Select(device => new AudioDevicePickerOption(device.Id, device.Name)));

        // A chosen device that is unplugged right now stays chosen (it comes back with the headset);
        // the list just shows it as absent, and the call takes Windows' device meanwhile.
        AudioDevicePickerOption? selected = options.FirstOrDefault(option =>
            string.Equals(option.DeviceId, selectedId, StringComparison.OrdinalIgnoreCase));
        if (selected is null && selectedId is not null)
        {
            selected = new AudioDevicePickerOption(selectedId, "Выбранное устройство (сейчас отключено)");
            options.Add(selected);
        }

        suppressAudioDeviceSelection = true;
        try
        {
            combo.ItemsSource = options;
            combo.SelectedItem = selected ?? options[0];
        }
        finally
        {
            suppressAudioDeviceSelection = false;
        }
    }

    private void UpdateAudioDevicesSummary(NativeWebRtcMediaEngine.AudioDeviceList devices)
    {
        SettingsAudioDevicesSummaryTextBlock.Text = (devices.Microphones.Count, devices.Speakers.Count) switch
        {
            (0, 0) => "Звуковые устройства не найдены.",
            (0, _) => "Микрофон не найден — собеседник вас не услышит, пока он не появится.",
            (_, 0) => "Динамики не найдены — собеседника не будет слышно, пока они не появятся.",
            _ => "Изменение применится к следующему звонку.",
        };
    }

    private void OnSettingsAudioDeviceSelectionChanged(object sender, SelectionChangedEventArgs e)
    {
        if (suppressAudioDeviceSelection || sender is not ComboBox { SelectedItem: AudioDevicePickerOption option } combo)
        {
            return;
        }

        bool microphone = ReferenceEquals(combo, SettingsMicrophoneComboBox);
        if (microphone)
        {
            callVideoService.PreferredMicrophoneId = option.DeviceId;
        }
        else
        {
            callVideoService.PreferredSpeakerId = option.DeviceId;
        }

        JsonObject document = ReadDevicePreferencesDocument();
        document[microphone ? MicrophonePreferenceKey : SpeakerPreferenceKey] = option.DeviceId;
        WriteDevicePreferencesDocument(document);

        string what = microphone ? "Микрофон" : "Динамики";
        SetActionStatus(option.DeviceId is null
            ? $"{what}: как в Windows. Применится к следующему звонку."
            : $"{what}: {option.DisplayName}. Применится к следующему звонку.");
    }

    private string? LoadPreferredAudioDeviceId(string key)
    {
        return ReadDevicePreferencesDocument()[key] is JsonValue value &&
            value.TryGetValue(out string? id) && !string.IsNullOrWhiteSpace(id)
            ? id
            : null;
    }

    // --- HW encode/decode device picker (Settings → "Аппаратное ускорение видео"): per-window
    // choice persisted in device-preferences.json. Options come from the live capability probe, so
    // only genuinely-capable GPUs are offered; "auto"/"software" are reserved. The chosen keys are
    // pushed to CallVideoService, which resolves them per session create. See task #87 ADR.

    private bool suppressCallVideoDeviceSelection;
    private bool callVideoDevicePickersInitialized;

    // Cheap, runs at startup: push the persisted encode/decode device keys to the media
    // service so the next call honours them. Deliberately does NOT run the capability probe —
    // that would load the Media Foundation / Intel-QSV / NVENC hardware stack (and leave its
    // threads + RAM resident) at boot just to fill Settings combos nobody is looking at yet.
    private void ApplyPersistedCallVideoDevicePreferences()
    {
        (string encodeKey, string decodeKey) = LoadDevicePreferenceKeys();
        callVideoService.SetDevicePreferences(encodeKey, decodeKey);
    }

    // Lazy, runs on first Settings view: populate the "Аппаратное ускорение видео" device
    // combos from the live capability probe. Deferred off the startup path (and off the UI
    // thread) so the HW-codec/camera enumeration only happens when the user actually opens
    // Settings — or after a call has already warmed the probe cache.
    private async void EnsureCallVideoDevicePickersPopulated()
    {
        if (callVideoDevicePickersInitialized)
        {
            return;
        }

        callVideoDevicePickersInitialized = true;
        try
        {
            (string encodeKey, string decodeKey) = LoadDevicePreferenceKeys();
            IReadOnlyList<NativeVideoDevice> devices =
                await Task.Run(() => NativeWebRtcMediaProbe.GetProbeResult().VideoDevices);
            PopulateCallVideoDeviceCombo(CallVideoEncodeDeviceComboBox, CallVideoMediaDirection.Encode, devices, encodeKey);
            PopulateCallVideoDeviceCombo(CallVideoDecodeDeviceComboBox, CallVideoMediaDirection.Decode, devices, decodeKey);
        }
        catch
        {
            // Probe failures already degrade to an empty device list inside the probe itself;
            // allow a retry on the next Settings view if something unexpected bubbled up here.
            callVideoDevicePickersInitialized = false;
        }
    }

    private void PopulateCallVideoDeviceCombo(
        ComboBox combo,
        CallVideoMediaDirection direction,
        IReadOnlyList<NativeVideoDevice> devices,
        string selectedKey)
    {
        IReadOnlyList<CallVideoDeviceOption> options = CallVideoDeviceSelection.BuildOptions(direction, devices);
        CallVideoDeviceOption selected = options
            .FirstOrDefault(option => string.Equals(option.Key, selectedKey, StringComparison.OrdinalIgnoreCase))
            ?? options[0];

        suppressCallVideoDeviceSelection = true;
        try
        {
            combo.DisplayMemberPath = nameof(CallVideoDeviceOption.DisplayName);
            combo.ItemsSource = options;
            combo.SelectedItem = selected;
        }
        finally
        {
            suppressCallVideoDeviceSelection = false;
        }
    }

    private void OnCallVideoDeviceSelectionChanged(object sender, SelectionChangedEventArgs e)
    {
        if (suppressCallVideoDeviceSelection)
        {
            return;
        }

        string encodeKey = (CallVideoEncodeDeviceComboBox.SelectedItem as CallVideoDeviceOption)?.Key
            ?? CallVideoDeviceSelection.AutoKey;
        string decodeKey = (CallVideoDecodeDeviceComboBox.SelectedItem as CallVideoDeviceOption)?.Key
            ?? CallVideoDeviceSelection.AutoKey;

        callVideoService.SetDevicePreferences(encodeKey, decodeKey);
        SaveDevicePreferenceKeys(encodeKey, decodeKey);
        SetActionStatus("Устройство видео обновлено. Применится при следующем звонке или перенастройке.");
    }

    private (string EncodeKey, string DecodeKey) LoadDevicePreferenceKeys()
    {
        try
        {
            string path = GetDevicePreferencesPath();
            if (!File.Exists(path))
            {
                return (CallVideoDeviceSelection.AutoKey, CallVideoDeviceSelection.AutoKey);
            }

            JsonNode? root = JsonNode.Parse(File.ReadAllText(path));
            string encode = root?["encodeDevice"]?.GetValue<string>() ?? CallVideoDeviceSelection.AutoKey;
            string decode = root?["decodeDevice"]?.GetValue<string>() ?? CallVideoDeviceSelection.AutoKey;
            return (
                string.IsNullOrWhiteSpace(encode) ? CallVideoDeviceSelection.AutoKey : encode,
                string.IsNullOrWhiteSpace(decode) ? CallVideoDeviceSelection.AutoKey : decode);
        }
        catch
        {
            return (CallVideoDeviceSelection.AutoKey, CallVideoDeviceSelection.AutoKey);
        }
    }

    private void SaveDevicePreferenceKeys(string encodeKey, string decodeKey)
    {
        JsonObject document = ReadDevicePreferencesDocument();
        document["encodeDevice"] = encodeKey;
        document["decodeDevice"] = decodeKey;
        WriteDevicePreferencesDocument(document);
    }

    private string GetDevicePreferencesPath()
        => Path.Combine(rustBridge.AppDirectory, "device-preferences.json");

    // Read-modify-write so camera and encode/decode keys never clobber each other.
    private JsonObject ReadDevicePreferencesDocument()
    {
        try
        {
            string path = GetDevicePreferencesPath();
            if (File.Exists(path) && JsonNode.Parse(File.ReadAllText(path)) is JsonObject existing)
            {
                return existing;
            }
        }
        catch
        {
            // Corrupt/unreadable file: start fresh rather than fail the preference write.
        }

        return new JsonObject();
    }

    private void WriteDevicePreferencesDocument(JsonObject document)
    {
        try
        {
            File.WriteAllText(GetDevicePreferencesPath(), document.ToJsonString());
        }
        catch
        {
            // Best-effort: if the app dir is unwritable the choice simply isn't sticky.
        }
    }

    private string? LoadPreferredCameraId()
    {
        try
        {
            string path = GetDevicePreferencesPath();
            if (!File.Exists(path))
            {
                return null;
            }

            JsonNode? root = JsonNode.Parse(File.ReadAllText(path));
            string? id = root?["cameraDeviceId"]?.GetValue<string>();
            return string.IsNullOrWhiteSpace(id) ? null : id;
        }
        catch
        {
            return null;
        }
    }

    private void SavePreferredCameraId(string? deviceId)
    {
        // Read-modify-write so we keep the encode/decode device keys in the same file.
        JsonObject document = ReadDevicePreferencesDocument();
        document["cameraDeviceId"] = deviceId;
        WriteDevicePreferencesDocument(document);
    }

    private void RefreshCallInbox()
    {
        if (!rustBridge.IsInitialized || windowState.Onboarding.ShellMode != AppShellMode.Authenticated)
        {
            currentCallInboxSnapshot = null;
            currentChatCallSummary = null;
            RenderIncomingCallBanner();
            RenderCurrentChatCallSummary();
            return;
        }

        try
        {
            ApplyCallInboxRefreshPayload(new CallInboxRefreshPayload(rustBridge.GetCallInboxSnapshot()));
        }
        catch
        {
            // Keep the last rendered call state if a transient refresh fails.
        }
    }

    private void ApplyCallInboxRefreshPayload(CallInboxRefreshPayload payload)
    {
        currentCallInboxSnapshot = payload.Snapshot;
        SyncCallMediaWithCallState();
        currentChatCallSummary = ResolveDisplayedChatCallSummary(GetResolvedCurrentDialogKey(), currentChatCallSummary);
        RenderIncomingCallBanner();
        RenderCurrentChatCallSummary();
    }

    /// <summary>
    /// Media follows the call state, in both directions and from ONE place: it comes up when the
    /// call is answered and goes down when the call is over. Every entry point into a call ends up
    /// here through the inbox refresh, so there is no second path that could disagree.
    /// </summary>
    private void SyncCallMediaWithCallState()
    {
        WarmMediaForPendingCall();
        StartCallMediaWhenCallIsAnswered();
        StopCallMediaIfCallIsOver();
        SyncRemoteControlSession();
    }

    /// <summary>
    /// A call is ringing on one end or the other — spend the expensive warm-ups now rather than
    /// after the answer. Cheap and idempotent: the probe only re-runs when its answer is stale.
    /// </summary>
    private void WarmMediaForPendingCall()
    {
        CallInboxSnapshot? inbox = currentCallInboxSnapshot;
        if (inbox is null)
        {
            return;
        }

        bool pendingCall =
            (inbox.CurrentCall is { } current &&
             CallOverlayPresenter.IsLiveCallState(current) &&
             !CallOverlayPresenter.IsAnsweredCallState(current)) ||
            inbox.IncomingCalls.Any(CallOverlayPresenter.IsLiveCallState);
        if (pendingCall)
        {
            bool remoteControl = inbox.CurrentCall?.RemoteControl is not null ||
                inbox.IncomingCalls.Any(call => call.RemoteControl is not null);
            callVideoService.WarmMediaProbesForPendingCall(cameraNeeded: !remoteControl);
        }
    }

    /// <summary>
    /// The call is answered — bring media up.
    ///
    /// The caller used to start media right after <c>PlaceCall</c>, so the microphone opened while
    /// the peer was still ringing and the machine pushed audio (and video, if the camera was on)
    /// at a dialog that had agreed to nothing. Measured 2026-08-11 on a call nobody picked up:
    /// 669 audio packets and 1281 video packets left the caller. The peer's media gate drops them,
    /// so nobody heard anything — but an open microphone before pickup is not something to leave
    /// standing on "the other side throws it away".
    ///
    /// Media is a property of an ANSWERED call, so it starts where the answer is observed. `active`
    /// is the only state that means answered: an outgoing call sits in `connecting` until
    /// CallAccept lands, and — because the liveness watchdog also fires on `connecting` — a call
    /// nobody picks up ends up in `reconnecting` without ever having been answered. Neither may
    /// open the microphone. `reconnecting` on an already-running call changes nothing here: media
    /// is already up, and only the call ending takes it down.
    /// </summary>
    /// The call the user just hung up — never (re)start media for it. See OnHangUpCurrentCallClicked.
    private string? hungUpCallId;

    private void StartCallMediaWhenCallIsAnswered()
    {
        CallSummary? liveCall = currentCallInboxSnapshot?.CurrentCall;
        if (liveCall is null || !CallOverlayPresenter.IsAnsweredCallState(liveCall))
        {
            return;
        }

        if (string.Equals(liveCall.CallId, hungUpCallId, StringComparison.Ordinal))
        {
            return; // we are ending it; a snapshot that still says "active" is just stale
        }

        string dialogKey = liveCall.DialogKey;
        // The refresh ticks every 500 ms while a call is live and StartAsync begins by tearing
        // down whatever media is running, so a second tick landing mid-start would stop the very
        // session it is bringing up. IsActiveForDialog only turns true once the receiver exists,
        // which is later than "a start is under way" — hence the explicit in-flight key. UI thread
        // only (the refresh coordinator marshals), so no interlock is needed.
        if (string.IsNullOrWhiteSpace(dialogKey) ||
            callMediaStartingForDialogKey is not null ||
            callVideoService.IsActiveForDialog(dialogKey))
        {
            return;
        }

        callMediaStartingForDialogKey = dialogKey;
        // A remote-control session is screen and input only: the microphone is never opened.
        callVideoService.MicrophoneDisabled = liveCall.RemoteControl is not null;
        callVideoService.CameraNotNeeded = liveCall.RemoteControl is not null;
        bool startMuted = liveCall.LocalMicMuted;
        _ = Task.Run(async () =>
        {
            string? failure = null;
            try
            {
                await callVideoService.StartAsync(dialogKey, startMuted);
            }
            catch (Exception ex)
            {
                failure = NormalizeUserError(ex);
            }

            _ = DispatcherQueue.TryEnqueue(() =>
            {
                callMediaStartingForDialogKey = null;
                if (failure is not null)
                {
                    SetActionStatus($"Медиа звонка не поднялось: {failure}");
                }

                RenderCurrentChatCallSummary();
                UpdateControlStates();
                // Media is up: an owner's remote-control session shares its screen now, not on
                // the next refresh tick (that tick was ~1.5 s of the time to a picture).
                SyncRemoteControlSession();
            });
        });
    }

    /// <summary>
    /// The peer hung up (or the call died) — tear our media down.
    ///
    /// Nothing used to do this: the shell only stopped media when the LOCAL user clicked,
    /// so a call ended from the other end left the microphone open, the overlay up and the
    /// peer's last decoded frame frozen on screen indefinitely. The core gets the end
    /// correctly (`call/in … applied: Completed` in the trace, row `ended` in the db); this
    /// is the shell finally following it.
    /// </summary>
    private void StopCallMediaIfCallIsOver()
    {
        string? mediaDialogKey = callVideoService.ActiveDialogKey;
        // A group call has no 1:1 call state at all — "no live 1:1 call" would kill it within
        // milliseconds of starting (found on the live 3-node pass, 2026-10-03).
        if (string.IsNullOrWhiteSpace(mediaDialogKey) || callVideoService.IsGroupMediaActive)
        {
            return;
        }

        CallSummary? liveCall = currentCallInboxSnapshot?.CurrentCall;
        bool callIsLive = liveCall is not null &&
                          CallOverlayPresenter.IsLiveCallState(liveCall.State) &&
                          string.Equals(liveCall.DialogKey, mediaDialogKey, StringComparison.Ordinal);
        if (callIsLive || !callVideoService.IsActiveForDialog(mediaDialogKey))
        {
            return;
        }

        bool wasRemoteControlSession = remoteControlSessionCallId is not null;
        _ = Task.Run(async () =>
        {
            try
            {
                await callVideoService.StopAsync();
            }
            catch
            {
                // Teardown is best-effort: the call is already over either way.
            }

            _ = DispatcherQueue.TryEnqueue(() =>
            {
                SetActionStatus(wasRemoteControlSession ? "Сеанс управления завершён." : "Звонок завершён собеседником.");
                RenderCurrentChatCallSummary();
                UpdateControlStates();
            });
        });
    }

    private TimeSpan GetCallRefreshCadence()
        => ShouldUseHotCallRefresh()
            ? CallRefreshHotCadence
            : CallRefreshIdleCadence;

    private bool ShouldUseHotCallRefresh()
        => CallOverlayPresenter.ShouldUseHotCallRefresh(currentCallInboxSnapshot);

    private CallSummary? ResolveDisplayedChatCallSummary(string? dialogKey, CallSummary? headerCallSummary)
        => CallOverlayPresenter.ResolveDisplayedChatCallSummary(
            currentCallInboxSnapshot, dialogKey, headerCallSummary);

    private CallSummary? GetIncomingBannerCall()
        => CallOverlayPresenter.GetIncomingBannerCall(currentCallInboxSnapshot);

    private CallSummary? GetActionableCurrentChatCall()
        => CallOverlayPresenter.GetActionableCurrentChatCall(
            currentCallInboxSnapshot, currentChatCallSummary, GetResolvedCurrentDialogKey());

    private void RenderIncomingCallBanner()
    {
        CallSummary? incomingCall = GetIncomingBannerCall();
        SurfaceWindowForIncomingCall(incomingCall);
        if (incomingCall is null)
        {
            IncomingCallBannerBorder.Visibility = Visibility.Collapsed;
            IncomingCallBannerTitleTextBlock.Text = "Входящий звонок";
            IncomingCallBannerSubtitleTextBlock.Text = "Когда появится входящий звонок, здесь можно будет быстро ответить.";
            return;
        }

        string caller = MainWindowPresentation.BuildCallPartySummary(incomingCall.Participants);
        if (incomingCall.RemoteControl is { IsOwner: true })
        {
            // A remote-control request (ask mode): say exactly what is being asked and how long the
            // question stays open — silence is a refusal.
            long left = incomingCall.RemoteControl.AnswerDeadlineUnixMs is long deadline
                ? Math.Max(0, (deadline - DateTimeOffset.UtcNow.ToUnixTimeMilliseconds()) / 1000)
                : 0;
            IncomingCallBannerTitleTextBlock.Text = $"«{caller}» хочет управлять вашим компьютером";
            IncomingCallBannerSubtitleTextBlock.Text = left > 0
                ? $"Увидит весь основной экран и сможет управлять мышью и клавиатурой. Без ответа запрос отклонится через {left} с."
                : "Увидит весь основной экран и сможет управлять мышью и клавиатурой.";
            AcceptIncomingCallButton.Content = "Разрешить на этот раз";
            AcceptIncomingCallMutedButton.Visibility = Visibility.Collapsed; // no microphone there
        }
        else
        {
            IncomingCallBannerTitleTextBlock.Text = $"{caller} звонит вам";
            IncomingCallBannerSubtitleTextBlock.Text = "Входящий звонок · сквозное шифрование";
            AcceptIncomingCallButton.Content = "Принять";
            AcceptIncomingCallMutedButton.Visibility = Visibility.Visible;
        }

        IncomingCallBannerBorder.Visibility = Visibility.Visible;
    }

    private void RenderCurrentChatCallSummary()
    {
        string? dialogKey = GetResolvedCurrentDialogKey();
        bool videoActive = callVideoService.IsActiveForDialog(dialogKey);
        CallOverlayState overlay = CallOverlayPresenter.Compute(
            currentChatCallSummary,
            videoActive,
            callVideoService.HasRemoteVideo,
            callVideoService.IsLocalCameraActive,
            callVideoService.ReadSilentMicrophone());
        ApplyCallOverlayState(overlay);
    }

    private void ApplyCallOverlayState(CallOverlayState overlay)
    {
        if (!overlay.Visible)
        {
            // The call is over (or there is none): the next one opens unfolded.
            callScreenMinimized = false;
        }

        bool shown = overlay.Visible && !callScreenMinimized;
        CallOverlayRoot.Visibility = shown ? Visibility.Visible : Visibility.Collapsed;
        CallIdentityPanel.Visibility = overlay.IdentityVisible ? Visibility.Visible : Visibility.Collapsed;
        CallPeerNameTextBlock.Text = overlay.PeerName;
        CallPeerAvatar.Text = overlay.AvatarText;
        // The overlay is the call of the open chat, so its photo is the one in the chat header.
        CallPeerAvatar.Picture = ChatIdentityAvatarImage.Visibility == Visibility.Visible ? ChatIdentityAvatarImage.Source : null;
        CallPeerAvatar.Fill = ChatIdentityAvatarCircle.Background;
        ChatCallStateTextBlock.Text = overlay.StateText;
        string details = string.IsNullOrEmpty(overlay.MicWarning) ? overlay.DetailsText : overlay.MicWarning;
        ChatCallDetailsTextBlock.Text = details;
        ChatCallDetailsTextBlock.Visibility = string.IsNullOrEmpty(details) ? Visibility.Collapsed : Visibility.Visible;
        ChatCallNetworkTextBlock.Text = overlay.NetworkText;
        ChatCallNetworkSeparator.Visibility = string.IsNullOrEmpty(overlay.NetworkText) ? Visibility.Collapsed : Visibility.Visible;
        // Over the picture the face card is gone: who and how long move to the top bar, and the
        // chrome fades while the mouse rests.
        CallTopBarInfo.Visibility = overlay.IdentityVisible ? Visibility.Collapsed : Visibility.Visible;
        CallTopBarNameTextBlock.Text = overlay.PeerName;
        CallTopBarStatusTextBlock.Text = string.IsNullOrEmpty(overlay.NetworkText)
            ? overlay.StateText
            : $"{overlay.StateText} · {overlay.NetworkText}";
        if (!string.IsNullOrEmpty(overlay.MicWarning))
        {
            // Over the picture the card is gone; the warning must not go with it.
            CallTopBarStatusTextBlock.Text += " · микрофон ничего не слышит";
        }
        CallOverlayHost.SetChromeAutoHide(shown && !overlay.IdentityVisible);
        ChatCallVideoGrid.Visibility = overlay.VideoGridVisible ? Visibility.Visible : Visibility.Collapsed;
        CallVideoDiagnosticsTextBlock.Visibility = overlay.VideoGridVisible ? Visibility.Visible : Visibility.Collapsed;
        // The identity card already says what we wait for; the placeholder line only speaks when the
        // card is not there — otherwise the two texts were drawn on top of each other.
        RemoteCallVideoPlaceholderTextBlock.Visibility = overlay.RemotePlaceholderVisible && !overlay.IdentityVisible
            ? Visibility.Visible
            : Visibility.Collapsed;
        // No live remote video (peer camera off / stream stalled — HasRemoteVideo is recency-gated):
        // hide the video sink so the peer avatar shows on the dark backdrop instead of the last
        // decoded frame frozen in place. BOTH sinks, not just the software one: the GPU swapchain
        // keeps presenting its last buffer forever regardless of what the Image does, so on the GPU
        // path the frozen frame stayed on screen — reported from the live test twice, once when the
        // peer stopped its camera and once when the call ended.
        RemoteCallVideoImage.Visibility = overlay.RemotePlaceholderVisible ? Visibility.Collapsed : Visibility.Visible;
        RemoteCallVideoSwapChainPanel.Visibility = overlay.RemotePlaceholderVisible ? Visibility.Collapsed : Visibility.Visible;
        // Hide our self-preview PiP while no local video source is running, so the corner shows
        // nothing rather than our last frame frozen in the box. `CameraOn` is now strictly the
        // CAMERA, so the screen-share case is added explicitly — you should still see what you
        // are broadcasting.
        LocalCallVideoPip.Visibility = overlay.CameraOn || callVideoService.IsScreenSharing
            ? Visibility.Visible
            : Visibility.Collapsed;
        AcceptCurrentCallButton.Visibility = overlay.AcceptVisible ? Visibility.Visible : Visibility.Collapsed;
        AcceptCurrentCallMutedButton.Visibility = overlay.AcceptMutedVisible ? Visibility.Visible : Visibility.Collapsed;
        DeclineCurrentCallButton.Visibility = overlay.DeclineVisible ? Visibility.Visible : Visibility.Collapsed;
        ToggleCurrentCallMuteButton.Visibility = overlay.MuteVisible ? Visibility.Visible : Visibility.Collapsed;
        ToggleCurrentCallMuteIcon.Glyph = ((char)(overlay.Muted ? 0xEC54 : 0xE720)).ToString();
        ToggleCurrentCallMuteButton.IsOn = overlay.Muted;
        Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(ToggleCurrentCallMuteButton, overlay.MuteAccessibleName);
        ToggleCurrentCallCameraButton.Visibility = overlay.CameraToggleVisible ? Visibility.Visible : Visibility.Collapsed;
        // Segoe MDL2 Assets (the FontIcon font) has no reliable "video off" glyph — F4F0 is
        // Segoe Fluent Icons only and renders as tofu here — so the camera toggle keeps the
        // Video glyph (E714) and says on/off by the fill (CallButton.IsOn: white = on).
        ToggleCurrentCallCameraIcon.Glyph = ((char)0xE714).ToString();
        ToggleCurrentCallCameraButton.IsOn = overlay.CameraOn;
        Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(ToggleCurrentCallCameraButton, overlay.CameraToggleAccessibleName);
        // Screen share shares the camera toggle's visibility (both are live-call video controls). Its
        // fill signals active-share; the accessible name flips to the stop action while sharing.
        bool sharing = callVideoService.IsScreenSharing;
        ToggleCurrentCallScreenShareButton.Visibility = overlay.CameraToggleVisible ? Visibility.Visible : Visibility.Collapsed;
        ToggleCurrentCallScreenShareButton.IsOn = sharing;
        Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(
            ToggleCurrentCallScreenShareButton,
            sharing ? "Остановить демонстрацию экрана" : "Демонстрация экрана");
        HangUpCurrentCallButton.Visibility = overlay.HangUpVisible ? Visibility.Visible : Visibility.Collapsed;
        Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(HangUpCurrentCallButton, overlay.HangUpAccessibleName);
        ToolTipService.SetToolTip(HangUpCurrentCallButton, overlay.RemoteControl ? overlay.HangUpAccessibleName : null);
        ToggleCallFullScreenButton.Visibility = overlay.FullScreenVisible ? Visibility.Visible : Visibility.Collapsed;
        RefreshCallScreenControlState();
        RenderActiveCallBar();
    }

    // The call screen folded into the strip (MinimizeCallButton); reset when the call is over.
    private bool callScreenMinimized;

    /// <summary>
    /// The strip over the window while a 1:1 call goes on out of sight: its screen is folded, or
    /// another chat (another section) is open. Tap — back to the call; mute and hang up in place.
    /// A call still ringing at us is the incoming banner's, not the strip's.
    /// </summary>
    private void RenderActiveCallBar()
    {
        CallSummary? call = currentCallInboxSnapshot?.CurrentCall;
        bool live = call is not null
            && CallOverlayPresenter.IsLiveCallState(call)
            && !CallOverlayPresenter.IsIncomingRingingCall(call);
        bool outOfSight = live && (callScreenMinimized || CallOverlayRoot.Visibility != Visibility.Visible);
        ActiveCallBar.Visibility = outOfSight ? Visibility.Visible : Visibility.Collapsed;
        if (!outOfSight || call is null)
        {
            return;
        }

        ActiveCallBarTextBlock.Text =
            $"{MainWindowPresentation.BuildCallPartySummary(call.Participants)} · {CallOverlayPresenter.BuildStatusLine(call)}";
        bool muteable = CallOverlayPresenter.IsMicToggleSupported(call);
        ActiveCallBarMuteButton.Visibility = muteable && call.RemoteControl is null ? Visibility.Visible : Visibility.Collapsed;
        ActiveCallBarMuteIcon.Glyph = ((char)(call.LocalMicMuted ? 0xEC54 : 0xE720)).ToString();
        Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(
            ActiveCallBarMuteButton,
            call.LocalMicMuted ? "Включить микрофон" : "Выключить микрофон");
    }

    private void OnMinimizeCallClicked(object sender, RoutedEventArgs e)
    {
        callScreenMinimized = true;
        RenderCurrentChatCallSummary();
    }

    private void OnReturnToCallClicked(object sender, RoutedEventArgs e)
    {
        callScreenMinimized = false;
        CallSummary? call = currentCallInboxSnapshot?.CurrentCall;
        if (call is not null && !string.Equals(call.DialogKey, GetResolvedCurrentDialogKey(), StringComparison.Ordinal))
        {
            // The call's own chat: its screen is that chat's.
            ShowChatsSectionForDialog(call.DialogKey, null);
            currentChatCallSummary = ResolveDisplayedChatCallSummary(call.DialogKey, currentChatCallSummary);
        }

        RenderCurrentChatCallSummary();
    }

    private void OnCallVideoServiceStateChanged(object? sender, EventArgs e)
    {
        // The event is raised from CallVideoService worker threads (encode loop,
        // render loop, profile-adapt callbacks). RenderCurrentChatCallSummary /
        // UpdateControlStates touch WinUI elements, which is invalid from any
        // non-UI thread. Previous code had a fallback that called these methods
        // directly on the worker thread when TryEnqueue returned false (e.g.
        // during dispatcher shutdown) — that path crashes with COMException
        // (RPC_E_WRONG_THREAD). The correct behavior on enqueue failure is to
        // drop the event silently; the service raises StateChanged often enough
        // that the next event will re-trigger render once the dispatcher is
        // available, or never (shutdown), which is fine.
        // See docs/bugs/video-pipeline-audit-2026-05-15.md (P0 #7).
        _ = DispatcherQueue.TryEnqueue(() =>
        {
            RenderCurrentChatCallSummary();
            UpdateControlStates();
        });
    }

    private void WatchCallVideoDialog(string dialogKey)
        => _ = Task.Run(async () =>
        {
            // Opening a 1:1 chat during a group call must not tear the group call's media down.
            if (callVideoService.IsGroupMediaActive)
            {
                return;
            }

            try
            {
                await callVideoService.WatchDialogAsync(dialogKey);
            }
            catch (Exception ex)
            {
                // Receive-setup failure must be visible to the user — architecture
                // forbids "fake active call state when V2 media transport is
                // unavailable". Surface the failure through the action status
                // bar via the dispatcher (we're on a worker thread here).
                // See docs/bugs/video-pipeline-audit-2026-05-15.md (P0 #8).
                string userMessage = NormalizeUserError(ex);
                _ = DispatcherQueue.TryEnqueue(() =>
                {
                    SetActionStatus($"Не удалось принимать видео: {userMessage}");
                });
            }
        });


    private void ShowChatsSectionForDialog(string dialogKey, string? contactId)
    {
        OpenChatThread(dialogKey, contactId);
        RefreshChatsList(true, dialogKey);
        AuthenticatedNavigationView.SelectedItem = ChatsNavigationItem;
        windowState.CurrentSection = AuthenticatedShellSection.Chats;
        ChatsSectionRoot.Visibility = Visibility.Visible;
        ContactsSectionRoot.Visibility = Visibility.Collapsed;
        SettingsSectionRoot.Visibility = Visibility.Collapsed;
        DiagnosticsSectionRoot.Visibility = Visibility.Collapsed;
    }

    private async void OnCallVideoPreferenceSelectionChanged(object sender, SelectionChangedEventArgs e)
    {
        if (suppressCallVideoPreferenceSelection)
        {
            return;
        }

        if (CallVideoFrameRateComboBox.SelectedItem is not CallVideoFrameRatePreferenceOption frameRateOption ||
            CallVideoCodecComboBox.SelectedItem is not CallVideoCodecPreferenceOption codecOption)
        {
            return;
        }

        try
        {
            // Разрешение сюда НЕ приходит: его выбирают на самой кнопке камеры/экрана, а здесь
            // остаются только кодек и частота кадров. Передаём текущее, чтобы не сбросить выбор.
            CallVideoResolutionOption currentResolution = callVideoService.GetPreferences().Resolution;
            await callVideoService.ApplyPreferencesAsync(
                codecOption.Preference,
                currentResolution,
                frameRateOption.Preference);
            SetActionStatus($"Формат видео: {frameRateOption.DisplayName}, {codecOption.DisplayName}.");
            RenderCurrentChatCallSummary();
            UpdateControlStates();
        }
        catch (Exception ex)
        {
            RefreshCallVideoPreferenceSelection();
            SetActionStatus($"Не удалось применить формат видео: {NormalizeUserError(ex)}");
        }
    }

    private async void OnStartCurrentChatCallClicked(object sender, RoutedEventArgs e)
    {
        string? dialogKey = GetApprovedCurrentDialogKey();
        if (string.IsNullOrWhiteSpace(dialogKey))
        {
            SetActionStatus("Звонок доступен только для подтверждённого контакта в открытом чате.");
            return;
        }

        try
        {
            // A call is the most latency-sensitive reach: if there's no mesh link to this
            // (already-open) chat's contact yet, nudge serverless rendezvous now rather than
            // wait for the periodic sweep (idempotent + gated in the core).
            TryReachContactOverRendezvous(currentChatHeaderSummary?.ContactCard?.Contact?.RemoteAccountId);
            // Place the call: creates the outgoing call-lifecycle + sends a CallInvite so the
            // peer rings. Media is NOT started here — it comes up when the peer answers, see
            // StartCallMediaWhenCallIsAnswered.
            await Task.Run(() => rustBridge.PlaceCall(dialogKey));
            currentChatCallSummary = null;
            SetActionStatus("Звонок запущен.");
            WakeCallRefresh();
            WakeChatRefresh();
            RefreshCallInbox();
            RefreshChatHeaderSummary();
            RenderCurrentChatCallSummary();
            UpdateControlStates();
        }
        catch (Exception ex)
        {
            SetActionStatus($"Не удалось запустить видео: {NormalizeUserError(ex)}");
        }
    }

    private async void OnStartSelectedContactCallClicked(object sender, RoutedEventArgs e)
    {
        if (string.IsNullOrWhiteSpace(windowState.SelectedContactId))
        {
            ContactManagementStatusTextBlock.Text = "Сначала выберите контакт.";
            return;
        }

        try
        {
            ContactChatTarget target = rustBridge.StartChatFromContact(windowState.SelectedContactId);
            ShowChatsSectionForDialog(target.DialogKey, target.ContactId);
            // Media is NOT started here — it comes up when the peer answers, see
            // StartCallMediaWhenCallIsAnswered.
            await Task.Run(() => rustBridge.PlaceCall(target.DialogKey));
            currentChatCallSummary = null;
            SetActionStatus("Звонок запущен.");
            WakeCallRefresh();
            WakeChatRefresh();
            RefreshCallInbox();
            RefreshChatHeaderSummary();
            RenderCurrentChatCallSummary();
            UpdateControlStates();
        }
        catch (Exception ex)
        {
            ContactManagementStatusTextBlock.Text = $"Не удалось запустить видео: {NormalizeUserError(ex)}";
        }
    }

    private void OnAcceptIncomingCallClicked(object sender, RoutedEventArgs e)
        => _ = AcceptIncomingCallAsync(micMuted: false);

    private void OnAcceptIncomingCallMutedClicked(object sender, RoutedEventArgs e)
        => _ = AcceptIncomingCallAsync(micMuted: true);

    private async Task AcceptIncomingCallAsync(bool micMuted)
    {
        CallSummary? incomingCall = GetIncomingBannerCall();
        if (incomingCall is null)
        {
            SetActionStatus("Входящий звонок уже исчез или был обработан.");
            return;
        }

        try
        {
            ShowChatsSectionForDialog(incomingCall.DialogKey, incomingCall.ContactId);
            if (micMuted)
            {
                // The mute goes into the core's call summary BEFORE the answer: the media comes up
                // on `active` and reads it from there (StartCallMediaWhenCallIsAnswered), so the
                // microphone opens muted instead of being muted a second later.
                await Task.Run(() => rustBridge.SetLocalMicMuted(incomingCall.CallId, true));
            }

            // Accept the call lifecycle (incoming -> active + send CallAccept so the caller goes
            // active too). Media follows the state: the RefreshCallInbox below sees `active` and
            // brings it up — the same single path the caller now takes.
            await Task.Run(() => rustBridge.AcceptCall(incomingCall.CallId));
            currentChatCallSummary = null;
            SetActionStatus(micMuted ? "Звонок принят, микрофон выключен." : "Звонок принят.");
            WakeCallRefresh();
            WakeChatRefresh();
            RefreshCallInbox();
            RefreshChatHeaderSummary();
            RenderCurrentChatCallSummary();
            UpdateControlStates();
        }
        catch (Exception ex)
        {
            SetActionStatus($"Не удалось принять звонок: {NormalizeUserError(ex)}");
        }
    }

    private async void OnDeclineIncomingCallClicked(object sender, RoutedEventArgs e)
    {
        CallSummary? incomingCall = GetIncomingBannerCall() ?? currentChatCallSummary;
        if (incomingCall is null)
        {
            SetActionStatus("Нет входящего звонка для отклонения.");
            return;
        }

        try
        {
            await Task.Run(() => rustBridge.DeclineCall(incomingCall.CallId, "declined"));
            SetActionStatus("Входящий звонок отклонён.");
            WakeCallRefresh();
            WakeChatRefresh();
            RefreshCallInbox();
            RefreshChatHeaderSummary();
            UpdateControlStates();
        }
        catch (Exception ex)
        {
            SetActionStatus($"Не удалось отклонить звонок: {NormalizeUserError(ex)}");
        }
    }

    private async void OnHangUpCurrentCallClicked(object sender, RoutedEventArgs e)
    {
        // Ending a call means BOTH: tear our media down and tell the peer. This used to
        // return early whenever media was live — so the first press stopped our own audio
        // and video and sent nothing, and the peer sat in a call that was over on our side
        // until the user pressed again. Reported from the live test as "сброс вызова привёл
        // к тому, что на второй стороне звонок всё ещё висел".
        CallSummary? currentCall = GetActionableCurrentChatCall();
        // The core hears it FIRST, then media goes. The other order left ~0.7 s in which our call
        // snapshot still said "active" with media already down — and the refresh tick restarted
        // media for a call we were ending (live 2026-10-06: capabilities re-published after the
        // hang-up, passive publishing for ~20 s more).
        hungUpCallId = currentCall?.CallId;
        try
        {
            if (currentCall is not null)
            {
                await Task.Run(() => rustBridge.HangUpCall(currentCall.CallId));
            }

            await callVideoService.StopAsync();

            currentChatCallSummary = null;
            SetActionStatus(currentCall is null ? "Видео завершено." : "Звонок завершён.");
            WakeCallRefresh();
            WakeChatRefresh();
            RefreshCallInbox();
            RefreshChatHeaderSummary();
            RenderCurrentChatCallSummary();
            UpdateControlStates();
        }
        catch (Exception ex)
        {
            SetActionStatus($"Не удалось завершить звонок: {NormalizeUserError(ex)}");
        }
    }

    private async void OnToggleCurrentCallMuteClicked(object sender, RoutedEventArgs e)
    {
        // This used to refuse to mute EXACTLY when the media session was live ("аудио в
        // текущем video path ещё не включено") — a leftover from before audio existed on
        // the native path. Audio has ridden it since Phase 2a, so the guard was inverted:
        // the mic could not be muted during a call, which is the only time it matters.
        CallSummary? currentCall = GetActionableCurrentChatCall();
        if (currentCall is null || !CallOverlayPresenter.IsMicToggleSupported(currentCall))
        {
            SetActionStatus("Микрофон можно переключать только во время звонка.");
            return;
        }

        bool nextMuted = !currentCall.LocalMicMuted;
        try
        {
            // Both halves: the core's call summary (what the UI and the peer's call state
            // read) AND the send loop's gate. Only the summary was being set, so "выключить
            // микрофон" changed an icon while the mic kept transmitting.
            callVideoService.SetLocalAudioMuted(nextMuted);
            CallSummary summary = await Task.Run(() => rustBridge.SetLocalMicMuted(currentCall.CallId, nextMuted));
            currentChatCallSummary = summary;
            SetActionStatus(nextMuted
                ? "Микрофон выключен."
                : "Микрофон снова включён.");
            WakeCallRefresh();
            RefreshCallInbox();
            RefreshChatHeaderSummary();
            UpdateControlStates();
        }
        catch (Exception ex)
        {
            SetActionStatus($"Не удалось переключить микрофон: {NormalizeUserError(ex)}");
        }
    }

    // Меню разрешения на самой кнопке. Показываем его в момент ВКЛЮЧЕНИЯ источника — это
    // единственный момент, когда выбор что-то меняет и когда человек о нём думает. Выключение
    // остаётся обычным нажатием.
    //
    // Список приходит от источника (профили камеры / размер монитора), текущий выбор помечен.
    // Если вариант всего один, меню не показываем вовсе: выбор без выбора — это лишний клик.
    private Task<CallVideoResolutionOption?> PickResolutionAsync(
        FrameworkElement anchor,
        IReadOnlyList<CallVideoResolutionOption> options,
        CallVideoResolutionOption current)
    {
        if (options.Count == 0)
        {
            return Task.FromResult<CallVideoResolutionOption?>(null);
        }

        if (options.Count == 1)
        {
            return Task.FromResult<CallVideoResolutionOption?>(options[0]);
        }

        TaskCompletionSource<CallVideoResolutionOption?> completion = new();
        MenuFlyout flyout = new();
        foreach (CallVideoResolutionOption option in options)
        {
            ToggleMenuFlyoutItem item = new()
            {
                Text = option.DisplayName,
                IsChecked = option == current,
            };
            item.Click += (_, _) => completion.TrySetResult(option);
            flyout.Items.Add(item);
        }

        // Закрыли меню мимо пунктов — значит передумали, а не выбрали первое попавшееся.
        flyout.Closed += (_, _) => completion.TrySetResult(null);
        flyout.ShowAt(anchor);
        return completion.Task;
    }

    private async Task<bool> TryApplyResolutionAsync(CallVideoResolutionOption resolution)
    {
        CallVideoPreferenceSnapshot preferences = callVideoService.GetPreferences();
        if (preferences.Resolution == resolution)
        {
            return true;
        }

        try
        {
            await callVideoService.ApplyPreferencesAsync(
                preferences.Codec,
                resolution,
                preferences.FrameRate);
            return true;
        }
        catch (Exception ex)
        {
            SetActionStatus($"Не удалось выбрать {resolution.DisplayName}: {NormalizeUserError(ex)}");
            return false;
        }
    }

    private async void OnToggleCurrentCallCameraClicked(object sender, RoutedEventArgs e)
    {
        // Audio-first: the camera is an opt-in toggle on the live native media session.
        if (!callVideoService.IsActiveForDialog(GetResolvedCurrentDialogKey()))
        {
            SetActionStatus("Камеру можно включить только во время активного звонка.");
            return;
        }

        bool turnOn = !callVideoService.IsLocalCameraActive;
        try
        {
            if (turnOn)
            {
                // Кнопка камеры ВКЛЮЧАЕТ КАМЕРУ. Здесь стоял обязательный выбор разрешения перед
                // каждым включением: нажал — получил меню. Меню было задумано как «выбор в момент,
                // когда он что-то меняет», но на деле оно встаёт поперёк единственного действия,
                // которого от кнопки ждут, и делает это каждый раз (сообщено 2026-08-30).
                // Разрешение осталось на той же кнопке — правой кнопкой, вторым жестом; пока его
                // не выбрали руками, оно само следует за возможностями камеры
                // (AdoptProbeDefaultResolutionUnsafe).
                await callVideoService.StartLocalCameraAsync();
                CallVideoResolutionOption current = callVideoService.GetPreferences().Resolution;
                SetActionStatus(callVideoService.IsLocalCameraActive
                    ? $"Камера включена: {current.DisplayName}. Качество — правой кнопкой по кнопке камеры."
                    : "Камера недоступна — звонок продолжается со звуком.");
            }
            else
            {
                await callVideoService.StopLocalCameraAsync();
                SetActionStatus("Камера выключена.");
            }

            UpdateControlStates();
        }
        catch (Exception ex)
        {
            SetActionStatus($"Не удалось переключить камеру: {NormalizeUserError(ex)}");
        }
    }

    /// Качество камеры — второй жест на той же кнопке (правая кнопка мыши, клавиша меню,
    /// долгое нажатие). Отдельного элемента в оверлее нет намеренно: контролов на звонке и так
    /// пять, а разрешение меняют редко. Пока сюда не заходили, разрешение следует за камерой само.
    private async void OnCurrentCallCameraQualityRequested(
        UIElement sender,
        Microsoft.UI.Xaml.Input.ContextRequestedEventArgs e)
    {
        // Иначе меню всплывёт ещё раз от родителя.
        e.Handled = true;
        if (!callVideoService.IsActiveForDialog(GetResolvedCurrentDialogKey()))
        {
            SetActionStatus("Качество видео выбирается во время активного звонка.");
            return;
        }

        try
        {
            IReadOnlyList<CallVideoResolutionOption> options =
                await callVideoService.GetCameraResolutionOptionsAsync();
            CallVideoResolutionOption? picked = await PickResolutionAsync(
                (FrameworkElement)sender,
                options,
                callVideoService.GetPreferences().Resolution);
            if (picked is null || !await TryApplyResolutionAsync(picked.Value))
            {
                return;
            }

            SetActionStatus($"Качество видео: {picked.Value.DisplayName}.");
            UpdateControlStates();
        }
        catch (Exception ex)
        {
            SetActionStatus($"Не удалось выбрать качество видео: {NormalizeUserError(ex)}");
        }
    }

    private async void OnToggleCurrentCallScreenShareClicked(object sender, RoutedEventArgs e)
    {
        // Screen share swaps the video source camera -> screen for the live 1:1 call. The peer just
        // sees the screen on the video track (in 1:1 it's visually obvious there's no separate signal).
        if (!callVideoService.IsActiveForDialog(GetResolvedCurrentDialogKey()))
        {
            SetActionStatus("Демонстрацию экрана можно включить только во время активного звонка.");
            return;
        }

        try
        {
            if (callVideoService.IsScreenSharing)
            {
                await callVideoService.StopScreenShareAsync();
                SetActionStatus("Демонстрация экрана остановлена.");
            }
            else
            {
                IntPtr hwnd = WinRT.Interop.WindowNative.GetWindowHandle(this);
                var item = await Features.Calls.ScreenShareCaptureSource.PickAsync(hwnd);
                if (item is not null)
                {
                    // Максимум у экрана — размер выбранного монитора или окна, и узнать его можно
                    // только ПОСЛЕ выбора источника, поэтому меню разрешения идёт вторым шагом.
                    // Размер экрана — свой, не камеры: по умолчанию родной, не выше 1440p; в настройку
                    // камеры он не пишется и камерой не проверяется.
                    IReadOnlyList<CallVideoResolutionOption> options =
                        CallVideoPreferences.BuildResolutionOptions(item.Size.Width, item.Size.Height);
                    CallVideoResolutionOption current =
                        callVideoService.LastScreenShareResolution is { } last && options.Contains(last)
                            ? last
                            : CallVideoPreferences.DefaultScreenResolution(options);
                    CallVideoResolutionOption? picked = await PickResolutionAsync(
                        (FrameworkElement)sender,
                        options,
                        current);
                    if (picked is null)
                    {
                        return;
                    }

                    await callVideoService.StartScreenShareAsync(item, picked.Value);
                    SetActionStatus(callVideoService.IsScreenSharing
                        ? $"Демонстрация экрана включена: {picked.Value.DisplayName}."
                        : "Демонстрация экрана недоступна — звонок продолжается со звуком.");
                }
            }

            UpdateControlStates();
        }
        catch (Exception ex)
        {
            SetActionStatus($"Не удалось переключить демонстрацию экрана: {NormalizeUserError(ex)}");
        }
    }
}
