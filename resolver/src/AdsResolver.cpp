#include "pxr/pxr.h"
#include "pxr/base/tf/diagnostic.h"
#include "pxr/usd/ar/defineResolver.h"
#include "pxr/usd/ar/filesystemAsset.h"
#include "pxr/usd/ar/filesystemWritableAsset.h"
#include "pxr/usd/ar/inMemoryAsset.h"
#include "pxr/usd/ar/notice.h"
#include "pxr/usd/ar/resolvedPath.h"
#include "pxr/usd/ar/resolver.h"

#include <algorithm>
#include <array>
#include <atomic>
#include <cctype>
#include <chrono>
#include <condition_variable>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <filesystem>
#include <fstream>
#include <iostream>
#include <memory>
#include <mutex>
#include <sstream>
#include <string>
#include <system_error>
#include <unordered_map>
#include <unordered_set>
#include <vector>

#if defined(_WIN32)
#ifndef NOMINMAX
#define NOMINMAX
#endif
#include <windows.h>
#include <winhttp.h>
#pragma comment(lib, "winhttp.lib")
#else
#include <unistd.h>
#endif

PXR_NAMESPACE_OPEN_SCOPE

namespace {

// Scheme matching policy: URI schemes are case-insensitive (RFC 3986), so
// `ADS://x` must route to this resolver just like `ads://x`. Ownership of a
// path is claimed case-insensitively on the `ads:` prefix (this predicate);
// actually resolving additionally requires the well-formed authority form
// `ads://` — malformed forms like `ads:foo` are rejected with a WarnOnce in
// ResolveAssetPath, the single chokepoint both _Resolve and _OpenAsset funnel
// through, so the two code paths always agree.
bool StartsWithAdsScheme(const std::string& value)
{
    if (value.size() < 4) {
        return false;
    }
    return (value[0] == 'a' || value[0] == 'A')
        && (value[1] == 'd' || value[1] == 'D')
        && (value[2] == 's' || value[2] == 'S')
        && value[3] == ':';
}

bool IsWellFormedAdsUri(const std::string& value)
{
    return StartsWithAdsScheme(value) && value.compare(4, 2, "//") == 0;
}

bool StartsWithHttpScheme(const std::string& value)
{
    return value.rfind("http://", 0) == 0 || value.rfind("https://", 0) == 0;
}

std::string StripSdfFormatArgs(std::string value)
{
    const std::string marker = ":SDF_FORMAT_ARGS:";
    const size_t markerPos = value.find(marker);
    if (markerPos != std::string::npos) {
        value.erase(markerPos);
    }
    return value;
}

std::string StripQuery(std::string value)
{
    const size_t queryPos = value.find('?');
    if (queryPos != std::string::npos) {
        value.erase(queryPos);
    }
    return value;
}

std::string ExtractQuery(const std::string& value)
{
    const size_t queryPos = value.find('?');
    if (queryPos == std::string::npos) {
        return {};
    }
    return value.substr(queryPos);
}

#if defined(_WIN32)
std::wstring Utf8ToWide(const std::string& value)
{
    if (value.empty()) {
        return {};
    }
    const int size = MultiByteToWideChar(
        CP_UTF8,
        0,
        value.data(),
        static_cast<int>(value.size()),
        nullptr,
        0);
    if (size <= 0) {
        return {};
    }
    std::wstring wide(static_cast<size_t>(size), L'\0');
    MultiByteToWideChar(
        CP_UTF8,
        0,
        value.data(),
        static_cast<int>(value.size()),
        wide.data(),
        size);
    return wide;
}

std::string WideToUtf8(const std::wstring& value)
{
    if (value.empty()) {
        return {};
    }
    const int size = WideCharToMultiByte(
        CP_UTF8,
        0,
        value.data(),
        static_cast<int>(value.size()),
        nullptr,
        0,
        nullptr,
        nullptr);
    if (size <= 0) {
        return {};
    }
    std::string utf8(static_cast<size_t>(size), '\0');
    WideCharToMultiByte(
        CP_UTF8,
        0,
        value.data(),
        static_cast<int>(value.size()),
        utf8.data(),
        size,
        nullptr,
        nullptr);
    return utf8;
}
#endif

// This resolver's internal string convention is UTF-8 — that is what USD
// expects of ArResolvedPath on Windows (ArchWindowsUtf8ToUtf16) and what the
// CLI prints on stdout. MSVC's std::filesystem narrow-path constructor
// decodes bytes with the ANSI code page instead, so every filesystem call
// routes through these helpers; otherwise a non-ASCII cache root (e.g. a
// Japanese user-profile path) would be written under one spelling and opened
// by USD under another.
std::filesystem::path ToFsPath(const std::string& utf8)
{
#if defined(_WIN32)
    return std::filesystem::path(Utf8ToWide(utf8));
#else
    return std::filesystem::path(utf8);
#endif
}

std::string FsPathToUtf8(const std::filesystem::path& path)
{
#if defined(_WIN32)
    return WideToUtf8(path.wstring());
#else
    return path.string();
#endif
}

std::string GetEnv(const char* name, const std::string& fallback = "")
{
#if defined(_WIN32)
    // CRT getenv returns ANSI-code-page bytes, which would poison the UTF-8
    // convention above for non-ASCII values (LOCALAPPDATA under a non-ASCII
    // user name, ADS_RESOLVER_WORKSPACE, ...). Read the wide environment and
    // convert.
    const std::wstring wideName = Utf8ToWide(name);
    if (wideName.empty()) {
        return fallback;
    }
    const DWORD size = GetEnvironmentVariableW(wideName.c_str(), nullptr, 0);
    if (size == 0) {
        return fallback;
    }
    std::wstring wide(static_cast<size_t>(size), L'\0');
    const DWORD written = GetEnvironmentVariableW(wideName.c_str(), wide.data(), size);
    if (written >= size) {
        return fallback;
    }
    wide.resize(written);
    return WideToUtf8(wide);
#else
    if (const char* value = std::getenv(name)) {
        return value;
    }
    return fallback;
#endif
}

bool DebugEnabled()
{
    const std::string value = GetEnv("ADS_RESOLVER_DEBUG");
    return value == "1" || value == "true" || value == "TRUE" || value == "yes";
}

// Maximum remote object size in bytes (ADS_RESOLVER_MAX_DOWNLOAD_MB,
// default 2048 MB). Zero disables the cap.
unsigned long long MaxDownloadBytes()
{
    const std::string value = GetEnv("ADS_RESOLVER_MAX_DOWNLOAD_MB", "2048");
    try {
        return std::stoull(value) * 1024ull * 1024ull;
    } catch (...) {
        return 2048ull * 1024ull * 1024ull;
    }
}

void LogResolverMessage(const std::string& message, bool always = false)
{
    if (!always && !DebugEnabled()) {
        return;
    }

    if (DebugEnabled()) {
        std::cerr << message << "\n";
    }

    const std::string logFile = GetEnv("ADS_RESOLVER_LOG_FILE");
    if (logFile.empty()) {
        return;
    }

    static std::mutex logMutex;
    std::lock_guard<std::mutex> lock(logMutex);
    std::ofstream stream(ToFsPath(logFile), std::ios::app);
    if (stream) {
        stream << message << "\n";
    }
}

// Origin (scheme://host[:port]) of a URL, used as a dedupe key: one dead
// object server fails for every layer URL on a stage, and the per-URL part
// would defeat deduplication.
std::string UrlOrigin(const std::string& url)
{
    const size_t schemeEnd = url.find("://");
    if (schemeEnd == std::string::npos) {
        return url;
    }
    const size_t pathStart = url.find('/', schemeEnd + 3);
    return pathStart == std::string::npos ? url : url.substr(0, pathStart);
}

// A stage with hundreds of ads:// layers hits the same dead server or bad
// token once per layer; emitting a TF_WARN for each would bury the one useful
// diagnostic in the host's console. Each distinct failure (kind + primary
// detail baked into dedupeKey) warns once per process; repeats still go
// through LogResolverMessage so ADS_RESOLVER_DEBUG / ADS_RESOLVER_LOG_FILE
// keep the full trace.
void WarnOnce(const std::string& dedupeKey, const std::string& message)
{
    static std::mutex warnMutex;
    static std::unordered_set<std::string> warnedKeys;

    bool firstOccurrence = false;
    {
        std::lock_guard<std::mutex> lock(warnMutex);
        firstOccurrence = warnedKeys.insert(dedupeKey).second;
    }
    if (firstOccurrence) {
        TF_WARN("%s", message.c_str());
    }
    LogResolverMessage(message, true);
}

std::string Trim(std::string value)
{
    auto notSpace = [](unsigned char ch) { return !std::isspace(ch); };
    value.erase(value.begin(), std::find_if(value.begin(), value.end(), notSpace));
    value.erase(std::find_if(value.rbegin(), value.rend(), notSpace).base(), value.end());
    return value;
}

std::string NormalizeSlashes(std::string value)
{
    std::replace(value.begin(), value.end(), '\\', '/');
    return value;
}

std::string NormalizeAdsUri(std::string uri)
{
    uri = NormalizeSlashes(std::move(uri));
    if (!StartsWithAdsScheme(uri)) {
        return uri;
    }
    // Lowercase the scheme so ADS://x and ads://x share one cache key and one
    // resolved identity.
    uri[0] = 'a';
    uri[1] = 'd';
    uri[2] = 's';
    const std::string prefix = "ads://";
    if (uri.rfind(prefix, 0) != 0) {
        // Malformed (no authority slashes, e.g. ads:foo); left untouched here
        // and rejected in ResolveAssetPath.
        return uri;
    }

    const std::string query = ExtractQuery(uri);
    std::string body = StripQuery(uri.substr(prefix.size()));
    std::vector<std::string> parts;
    std::stringstream stream(body);
    std::string part;
    while (std::getline(stream, part, '/')) {
        if (part.empty() || part == ".") {
            continue;
        }
        if (part == "..") {
            if (!parts.empty()) {
                parts.pop_back();
            }
            continue;
        }
        parts.push_back(part);
    }

    std::ostringstream normalized;
    normalized << prefix;
    for (size_t index = 0; index < parts.size(); ++index) {
        if (index > 0) {
            normalized << '/';
        }
        normalized << parts[index];
    }
    normalized << query;
    return normalized.str();
}

std::string CreateAdsRelativeIdentifier(
    const std::string& assetPath,
    const ArResolvedPath& anchorAssetPath)
{
    std::string anchor = NormalizeSlashes(StripSdfFormatArgs(anchorAssetPath.GetPathString()));
    if (!StartsWithAdsScheme(anchor)) {
        return {};
    }

    std::string relative = NormalizeSlashes(StripSdfFormatArgs(assetPath));
    const std::string query = ExtractQuery(relative).empty() ? ExtractQuery(anchor) : ExtractQuery(relative);
    anchor = StripQuery(anchor);
    relative = StripQuery(relative);

    const size_t slashPos = anchor.rfind('/');
    const std::string base = slashPos == std::string::npos ? anchor : anchor.substr(0, slashPos + 1);
    return NormalizeAdsUri(base + relative + query);
}

std::string QuoteProcessArg(const std::string& value)
{
    std::string quoted = "\"";
    unsigned backslashes = 0;
    for (char ch : value) {
        if (ch == '\\') {
            ++backslashes;
            continue;
        }
        if (ch == '"') {
            quoted.append(backslashes * 2 + 1, '\\');
            quoted.push_back('"');
        } else {
            quoted.append(backslashes, '\\');
            quoted.push_back(ch);
        }
        backslashes = 0;
    }
    quoted.append(backslashes * 2, '\\');
    quoted.push_back('"');
    return quoted;
}

std::string ShellQuote(const std::string& value)
{
#if defined(_WIN32)
    return QuoteProcessArg(value);
#else
    std::string quoted = "'";
    for (char ch : value) {
        if (ch == '\'') {
            quoted += "'\\''";
        } else {
            quoted.push_back(ch);
        }
    }
    quoted.push_back('\'');
    return quoted;
#endif
}

std::string JoinProcessCommandLine(const std::vector<std::string>& args)
{
    std::ostringstream command;
    for (size_t index = 0; index < args.size(); ++index) {
        if (index > 0) {
            command << ' ';
        }
        command << QuoteProcessArg(args[index]);
    }
    return command.str();
}

std::string JoinShellCommand(const std::vector<std::string>& args)
{
    std::ostringstream command;
    for (size_t index = 0; index < args.size(); ++index) {
        if (index > 0) {
            command << ' ';
        }
        command << ShellQuote(args[index]);
    }
    command << " 2>/dev/null";
    return command.str();
}

#if defined(_WIN32)
struct ScopedHandle
{
    HANDLE handle { nullptr };

    ScopedHandle() = default;
    explicit ScopedHandle(HANDLE value) : handle(value) {}
    ~ScopedHandle()
    {
        if (handle && handle != INVALID_HANDLE_VALUE) {
            CloseHandle(handle);
        }
    }

    ScopedHandle(const ScopedHandle&) = delete;
    ScopedHandle& operator=(const ScopedHandle&) = delete;

    HANDLE get() const
    {
        return handle;
    }

    HANDLE release()
    {
        HANDLE released = handle;
        handle = nullptr;
        return released;
    }

    void reset(HANDLE value = nullptr)
    {
        if (handle && handle != INVALID_HANDLE_VALUE) {
            CloseHandle(handle);
        }
        handle = value;
    }
};

struct ScopedWinHttpHandle
{
    HINTERNET handle { nullptr };

    ScopedWinHttpHandle() = default;
    explicit ScopedWinHttpHandle(HINTERNET value) : handle(value) {}
    ~ScopedWinHttpHandle()
    {
        if (handle) {
            WinHttpCloseHandle(handle);
        }
    }

    ScopedWinHttpHandle(const ScopedWinHttpHandle&) = delete;
    ScopedWinHttpHandle& operator=(const ScopedWinHttpHandle&) = delete;

    HINTERNET get() const
    {
        return handle;
    }
};

// Whole-lifetime deadline for one CLI child process (spawn -> stdout EOF ->
// exit), so a hung ads.exe cannot hang a USD composition thread forever
// (ADS_RESOLVER_CLI_TIMEOUT_SECONDS, default 60).
DWORD CliTimeoutMilliseconds()
{
    const std::string value = GetEnv("ADS_RESOLVER_CLI_TIMEOUT_SECONDS", "60");
    try {
        const long seconds = std::stol(value);
        if (seconds > 0) {
            return static_cast<DWORD>(std::min(seconds, 86400L)) * 1000;
        }
    } catch (...) {
    }
    return 60 * 1000;
}
#endif

bool ReadCommandBytes(const std::vector<std::string>& args, std::vector<char>* output)
{
    std::array<char, 64 * 1024> buffer {};

#if defined(_WIN32)
    // Anonymous pipes cannot do overlapped I/O, so the read end is a
    // single-use named-pipe server with a process-unique name. Overlapped
    // reads let the wait below be bounded by a deadline instead of blocking
    // in ReadFile until an arbitrary child exits.
    static std::atomic<unsigned long> pipeSerial { 0 };
    std::wostringstream pipeNameStream;
    pipeNameStream << L"\\\\.\\pipe\\ads-resolver-" << GetCurrentProcessId() << L"-"
                   << pipeSerial.fetch_add(1);
    const std::wstring pipeName = pipeNameStream.str();

    ScopedHandle readPipe(CreateNamedPipeW(
        pipeName.c_str(),
        PIPE_ACCESS_INBOUND | FILE_FLAG_OVERLAPPED | FILE_FLAG_FIRST_PIPE_INSTANCE,
        PIPE_TYPE_BYTE | PIPE_WAIT,
        1,
        static_cast<DWORD>(buffer.size()),
        static_cast<DWORD>(buffer.size()),
        0,
        nullptr));
    if (readPipe.get() == INVALID_HANDLE_VALUE) {
        return false;
    }

    SECURITY_ATTRIBUTES inheritable {};
    inheritable.nLength = sizeof(inheritable);
    inheritable.bInheritHandle = TRUE;

    ScopedHandle writePipe(CreateFileW(
        pipeName.c_str(),
        GENERIC_WRITE,
        0,
        &inheritable,
        OPEN_EXISTING,
        0,
        nullptr));
    if (writePipe.get() == INVALID_HANDLE_VALUE) {
        return false;
    }

    // The child gets NUL for stdin/stderr rather than the parent's handles:
    // ads resolve reads nothing, and the parent's std handles may not be
    // inheritable (or may not exist in a GUI host like Houdini).
    ScopedHandle nulInput(CreateFileW(
        L"NUL",
        GENERIC_READ,
        FILE_SHARE_READ | FILE_SHARE_WRITE,
        &inheritable,
        OPEN_EXISTING,
        FILE_ATTRIBUTE_NORMAL,
        nullptr));
    ScopedHandle nulError(CreateFileW(
        L"NUL",
        GENERIC_WRITE,
        FILE_SHARE_READ | FILE_SHARE_WRITE,
        &inheritable,
        OPEN_EXISTING,
        FILE_ATTRIBUTE_NORMAL,
        nullptr));

    std::wstring commandLine = Utf8ToWide(JoinProcessCommandLine(args));
    if (commandLine.empty()) {
        return false;
    }
    std::vector<wchar_t> mutableCommandLine(commandLine.begin(), commandLine.end());
    mutableCommandLine.push_back(L'\0');

    // PROC_THREAD_ATTRIBUTE_HANDLE_LIST restricts inheritance to exactly the
    // handles this child needs. Without it, concurrent USD threads spawning
    // ads.exe leak each other's pipe write-ends into unrelated children, and
    // ReadFile never sees EOF until the unrelated child exits too.
    std::vector<HANDLE> inheritHandles;
    inheritHandles.push_back(writePipe.get());
    if (nulInput.get() != INVALID_HANDLE_VALUE) {
        inheritHandles.push_back(nulInput.get());
    }
    if (nulError.get() != INVALID_HANDLE_VALUE) {
        inheritHandles.push_back(nulError.get());
    }

    SIZE_T attributeListSize = 0;
    InitializeProcThreadAttributeList(nullptr, 1, 0, &attributeListSize);
    if (attributeListSize == 0) {
        return false;
    }
    std::vector<unsigned char> attributeListStorage(attributeListSize);
    auto* attributeList =
        reinterpret_cast<LPPROC_THREAD_ATTRIBUTE_LIST>(attributeListStorage.data());
    if (!InitializeProcThreadAttributeList(attributeList, 1, 0, &attributeListSize)) {
        return false;
    }
    if (!UpdateProcThreadAttribute(
            attributeList,
            0,
            PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
            inheritHandles.data(),
            inheritHandles.size() * sizeof(HANDLE),
            nullptr,
            nullptr)) {
        DeleteProcThreadAttributeList(attributeList);
        return false;
    }

    STARTUPINFOEXW startupInfo {};
    startupInfo.StartupInfo.cb = sizeof(startupInfo);
    startupInfo.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    startupInfo.StartupInfo.hStdInput =
        nulInput.get() == INVALID_HANDLE_VALUE ? nullptr : nulInput.get();
    startupInfo.StartupInfo.hStdOutput = writePipe.get();
    startupInfo.StartupInfo.hStdError =
        nulError.get() == INVALID_HANDLE_VALUE ? writePipe.get() : nulError.get();
    startupInfo.lpAttributeList = attributeList;

    PROCESS_INFORMATION processInfo {};
    const BOOL created = CreateProcessW(
        nullptr,
        mutableCommandLine.data(),
        nullptr,
        nullptr,
        TRUE,
        CREATE_NO_WINDOW | EXTENDED_STARTUPINFO_PRESENT,
        nullptr,
        nullptr,
        &startupInfo.StartupInfo,
        &processInfo);
    DeleteProcThreadAttributeList(attributeList);

    // Close the parent's copies so the child holds the only write end; its
    // exit then breaks the pipe and the read loop sees EOF.
    writePipe.reset();
    nulInput.reset();
    nulError.reset();

    if (!created) {
        return false;
    }

    ScopedHandle process(processInfo.hProcess);
    ScopedHandle thread(processInfo.hThread);

    ScopedHandle readEvent(CreateEventW(nullptr, TRUE, FALSE, nullptr));
    if (!readEvent.get()) {
        TerminateProcess(process.get(), 1);
        return false;
    }

    const ULONGLONG deadline = GetTickCount64() + CliTimeoutMilliseconds();
    const auto remainingMs = [deadline]() -> DWORD {
        const ULONGLONG now = GetTickCount64();
        return now >= deadline ? 0 : static_cast<DWORD>(deadline - now);
    };

    output->clear();
    bool timedOut = false;
    bool readFailed = false;
    while (true) {
        OVERLAPPED overlapped {};
        overlapped.hEvent = readEvent.get();
        BOOL pending = FALSE;
        if (!ReadFile(
                readPipe.get(),
                buffer.data(),
                static_cast<DWORD>(buffer.size()),
                nullptr,
                &overlapped)) {
            const DWORD readError = GetLastError();
            if (readError == ERROR_BROKEN_PIPE) {
                break;
            }
            if (readError != ERROR_IO_PENDING) {
                readFailed = true;
                break;
            }
            pending = TRUE;
        }
        if (pending) {
            HANDLE waitHandles[2] = { readEvent.get(), process.get() };
            DWORD waitResult =
                WaitForMultipleObjects(2, waitHandles, FALSE, remainingMs());
            if (waitResult == WAIT_OBJECT_0 + 1) {
                // Child exited: the kernel closed its write end, so the pending
                // read now drains buffered bytes and then breaks the pipe. Keep
                // waiting on the read alone, still under the deadline (covers a
                // grandchild that inherited the write end from the child).
                waitResult = WaitForSingleObject(readEvent.get(), remainingMs());
            }
            if (waitResult != WAIT_OBJECT_0) {
                timedOut = true;
                // The pending read still targets the stack buffer; cancel and
                // drain the completion before the buffer goes out of scope.
                CancelIo(readPipe.get());
                DWORD ignored = 0;
                GetOverlappedResult(readPipe.get(), &overlapped, &ignored, TRUE);
                break;
            }
        }
        DWORD bytesRead = 0;
        if (!GetOverlappedResult(readPipe.get(), &overlapped, &bytesRead, FALSE)) {
            if (GetLastError() == ERROR_BROKEN_PIPE) {
                break;
            }
            readFailed = true;
            break;
        }
        if (bytesRead > 0) {
            output->insert(output->end(), buffer.data(), buffer.data() + bytesRead);
        }
    }

    if (!timedOut && !readFailed) {
        if (WaitForSingleObject(process.get(), remainingMs()) != WAIT_OBJECT_0) {
            timedOut = true;
        }
    }

    if (timedOut || readFailed) {
        TerminateProcess(process.get(), 1);
        if (timedOut) {
            WarnOnce(
                "cli-timeout\n" + args[0],
                "ADS Resolver: command `" + JoinProcessCommandLine(args)
                    + "` did not finish within ADS_RESOLVER_CLI_TIMEOUT_SECONDS; "
                      "terminated and treated as a failed resolution");
        }
        output->clear();
        return false;
    }

    DWORD exitCode = 1;
    GetExitCodeProcess(process.get(), &exitCode);
    if (exitCode != 0) {
        output->clear();
        return false;
    }
    return true;
#else
    const std::string command = JoinShellCommand(args);
    FILE* pipe = popen(command.c_str(), "r");
    if (!pipe) {
        return false;
    }

    output->clear();
    while (true) {
        const size_t bytesRead = std::fread(buffer.data(), 1, buffer.size(), pipe);
        if (bytesRead > 0) {
            output->insert(output->end(), buffer.data(), buffer.data() + bytesRead);
        }
        if (bytesRead < buffer.size()) {
            if (std::feof(pipe)) {
                break;
            }
            if (std::ferror(pipe)) {
                pclose(pipe);
                output->clear();
                return false;
            }
        }
    }

    const int status = pclose(pipe);
    if (status != 0) {
        output->clear();
        return false;
    }
    return true;
#endif
}

std::string ReadCommandStdout(const std::vector<std::string>& args)
{
    std::vector<char> output;
    if (!ReadCommandBytes(args, &output)) {
        return {};
    }
    return Trim(std::string(output.begin(), output.end()));
}

#if defined(_WIN32)
DWORD TimeoutMilliseconds(const std::string& timeoutSeconds)
{
    if (timeoutSeconds.empty()) {
        return 30 * 1000;
    }
    try {
        const int seconds = std::stoi(timeoutSeconds);
        if (seconds <= 0) {
            return 30 * 1000;
        }
        return static_cast<DWORD>(seconds * 1000);
    } catch (...) {
        return 30 * 1000;
    }
}

bool HttpGetBytesNative(
    const std::string& url,
    const std::string& bearerToken,
    const std::string& timeoutSeconds,
    std::vector<char>* output)
{
    const std::wstring wideUrl = Utf8ToWide(url);
    if (wideUrl.empty()) {
        return false;
    }

    URL_COMPONENTSW components {};
    components.dwStructSize = sizeof(components);
    components.dwSchemeLength = static_cast<DWORD>(-1);
    components.dwHostNameLength = static_cast<DWORD>(-1);
    components.dwUrlPathLength = static_cast<DWORD>(-1);
    components.dwExtraInfoLength = static_cast<DWORD>(-1);
    if (!WinHttpCrackUrl(wideUrl.c_str(), 0, 0, &components)) {
        return false;
    }
    if (components.nScheme != INTERNET_SCHEME_HTTP && components.nScheme != INTERNET_SCHEME_HTTPS) {
        return false;
    }

    const std::wstring host(components.lpszHostName, components.dwHostNameLength);
    std::wstring path(components.lpszUrlPath, components.dwUrlPathLength);
    path.append(components.lpszExtraInfo, components.dwExtraInfoLength);
    if (path.empty()) {
        path = L"/";
    }

    ScopedWinHttpHandle session(WinHttpOpen(
        L"ADSResolver/0.1",
        WINHTTP_ACCESS_TYPE_DEFAULT_PROXY,
        WINHTTP_NO_PROXY_NAME,
        WINHTTP_NO_PROXY_BYPASS,
        0));
    if (!session.get()) {
        return false;
    }

    const DWORD timeout = TimeoutMilliseconds(timeoutSeconds);
    WinHttpSetTimeouts(session.get(), timeout, timeout, timeout, timeout);

    ScopedWinHttpHandle connection(WinHttpConnect(session.get(), host.c_str(), components.nPort, 0));
    if (!connection.get()) {
        return false;
    }

    const DWORD flags = components.nScheme == INTERNET_SCHEME_HTTPS ? WINHTTP_FLAG_SECURE : 0;
    ScopedWinHttpHandle request(WinHttpOpenRequest(
        connection.get(),
        L"GET",
        path.c_str(),
        nullptr,
        WINHTTP_NO_REFERER,
        WINHTTP_DEFAULT_ACCEPT_TYPES,
        flags));
    if (!request.get()) {
        return false;
    }

    if (!bearerToken.empty()) {
        const std::wstring header = Utf8ToWide("Authorization: Bearer " + bearerToken);
        if (header.empty()
            || !WinHttpAddRequestHeaders(
                request.get(),
                header.c_str(),
                static_cast<DWORD>(-1),
                WINHTTP_ADDREQ_FLAG_ADD | WINHTTP_ADDREQ_FLAG_REPLACE)) {
            return false;
        }
    }

    if (!WinHttpSendRequest(
            request.get(),
            WINHTTP_NO_ADDITIONAL_HEADERS,
            0,
            WINHTTP_NO_REQUEST_DATA,
            0,
            0,
            0)
        || !WinHttpReceiveResponse(request.get(), nullptr)) {
        const DWORD lastError = GetLastError();
        WarnOnce(
            "http-request\n" + UrlOrigin(url),
            "ADS Resolver: HTTP request to `" + url + "` failed (WinHTTP error "
                + std::to_string(lastError)
                + "); check that the server is reachable and ADS_RESOLVER_SERVER is correct");
        return false;
    }

    DWORD statusCode = 0;
    DWORD statusCodeSize = sizeof(statusCode);
    if (!WinHttpQueryHeaders(
            request.get(),
            WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
            WINHTTP_HEADER_NAME_BY_INDEX,
            &statusCode,
            &statusCodeSize,
            WINHTTP_NO_HEADER_INDEX)) {
        return false;
    }
    if (statusCode < 200 || statusCode >= 300) {
        WarnOnce(
            "http-status\n" + std::to_string(statusCode) + "\n" + UrlOrigin(url),
            "ADS Resolver: HTTP GET `" + url + "` returned status "
                + std::to_string(statusCode)
                + (statusCode == 401 || statusCode == 403
                       ? "; check ADS_RESOLVER_API_TOKEN / bearer token"
                       : ""));
        return false;
    }

    // Remote reads are buffered fully in memory (ArInMemoryAsset), so cap the
    // download size to keep one runaway object from taking the host process
    // down. Checked against Content-Length up front and enforced during
    // chunked reads.
    const unsigned long long maxBytes = MaxDownloadBytes();
    if (maxBytes != 0) {
        wchar_t contentLength[32] = {};
        DWORD contentLengthSize = sizeof(contentLength);
        if (WinHttpQueryHeaders(
                request.get(),
                WINHTTP_QUERY_CONTENT_LENGTH,
                WINHTTP_HEADER_NAME_BY_INDEX,
                contentLength,
                &contentLengthSize,
                WINHTTP_NO_HEADER_INDEX)) {
            const unsigned long long announced = std::wcstoull(contentLength, nullptr, 10);
            if (announced > maxBytes) {
                WarnOnce(
                    "download-cap\n" + url,
                    "ADS Resolver: refused remote object `" + url + "`: Content-Length "
                        + std::to_string(announced) + " exceeds ADS_RESOLVER_MAX_DOWNLOAD_MB ("
                        + std::to_string(maxBytes) + " bytes)");
                return false;
            }
        }
    }

    output->clear();
    while (true) {
        DWORD available = 0;
        if (!WinHttpQueryDataAvailable(request.get(), &available)) {
            output->clear();
            return false;
        }
        if (available == 0) {
            break;
        }
        if (maxBytes != 0
            && static_cast<unsigned long long>(output->size()) + available > maxBytes) {
            WarnOnce(
                "download-cap\n" + url,
                "ADS Resolver: aborted remote object download `" + url
                    + "`: size exceeds ADS_RESOLVER_MAX_DOWNLOAD_MB ("
                    + std::to_string(maxBytes) + " bytes)");
            output->clear();
            return false;
        }
        const size_t offset = output->size();
        output->resize(offset + available);

        DWORD bytesRead = 0;
        if (!WinHttpReadData(
                request.get(),
                output->data() + offset,
                available,
                &bytesRead)) {
            output->clear();
            return false;
        }
        output->resize(offset + bytesRead);
    }
    return true;
}
#endif

bool HttpGetBytes(
    const std::string& url,
    const std::string& bearerToken,
    const std::string& timeoutSeconds,
    std::vector<char>* output)
{
#if defined(_WIN32)
    return HttpGetBytesNative(url, bearerToken, timeoutSeconds, output);
#else
    const std::string executable = GetEnv("ADS_RESOLVER_HTTP_EXECUTABLE", "curl");
    std::vector<std::string> args {
        executable,
        "--location",
        "--fail",
        "--silent",
        "--show-error",
    };
    if (!timeoutSeconds.empty()) {
        args.push_back("--max-time");
        args.push_back(timeoutSeconds);
    }
    // curl only honors --max-filesize when the server announces Content-Length;
    // chunked/streamed responses pass through unchecked. Cumulative enforcement
    // (as in the WinHTTP path) would require replacing the popen capture with a
    // native HTTP backend, so this stays best-effort until then.
    const unsigned long long maxBytes = MaxDownloadBytes();
    if (maxBytes != 0) {
        args.push_back("--max-filesize");
        args.push_back(std::to_string(maxBytes));
    }
    if (!bearerToken.empty()) {
        args.push_back("--header");
        args.push_back("Authorization: Bearer " + bearerToken);
    }
    args.push_back(url);
    return ReadCommandBytes(args, output);
#endif
}

std::string HttpGetText(
    const std::string& url,
    const std::string& bearerToken,
    const std::string& timeoutSeconds)
{
    std::vector<char> bytes;
    if (!HttpGetBytes(url, bearerToken, timeoutSeconds, &bytes)) {
        return {};
    }
    return std::string(bytes.begin(), bytes.end());
}

std::string TrimTrailingSlashes(std::string value)
{
    while (!value.empty() && value.back() == '/') {
        value.pop_back();
    }
    return value;
}

std::string PercentEncode(const std::string& value)
{
    std::ostringstream encoded;
    encoded << std::uppercase << std::hex;
    for (const unsigned char byte : value) {
        if ((byte >= 'A' && byte <= 'Z') || (byte >= 'a' && byte <= 'z')
            || (byte >= '0' && byte <= '9') || byte == '-' || byte == '_' || byte == '.'
            || byte == '~') {
            encoded << static_cast<char>(byte);
        } else {
            encoded << '%' << static_cast<int>(byte >> 4) << static_cast<int>(byte & 0x0f);
        }
    }
    return encoded.str();
}

std::string JsonStringField(const std::string& json, const std::string& field)
{
    const std::string needle = "\"" + field + "\"";
    size_t pos = json.find(needle);
    if (pos == std::string::npos) {
        return {};
    }
    pos = json.find(':', pos + needle.size());
    if (pos == std::string::npos) {
        return {};
    }
    pos = json.find('"', pos + 1);
    if (pos == std::string::npos) {
        return {};
    }
    ++pos;

    std::string value;
    bool escaped = false;
    for (; pos < json.size(); ++pos) {
        const char ch = json[pos];
        if (escaped) {
            switch (ch) {
                case '"':
                case '\\':
                case '/':
                    value.push_back(ch);
                    break;
                case 'b':
                    value.push_back('\b');
                    break;
                case 'f':
                    value.push_back('\f');
                    break;
                case 'n':
                    value.push_back('\n');
                    break;
                case 'r':
                    value.push_back('\r');
                    break;
                case 't':
                    value.push_back('\t');
                    break;
                default:
                    value.push_back(ch);
                    break;
            }
            escaped = false;
            continue;
        }
        if (ch == '\\') {
            escaped = true;
            continue;
        }
        if (ch == '"') {
            return value;
        }
        value.push_back(ch);
    }
    return {};
}

// Schema v8 cache policy: an explicit version pin resolves to an immutable
// manifest, so the result may be cached for the whole session. current/latest
// are mutable pointers and only stay cached for a short TTL so pointer
// switches on the server become visible without restarting the host process.
struct ResolveCacheEntry
{
    std::string location;
    std::chrono::steady_clock::time_point expiry;
    bool permanent = false;
};

std::string VersionSelectorValue(const std::string& assetPath)
{
    const std::size_t query = assetPath.find('?');
    if (query == std::string::npos) {
        return {};
    }
    std::size_t position = query + 1;
    while (position < assetPath.size()) {
        std::size_t end = assetPath.find('&', position);
        if (end == std::string::npos) {
            end = assetPath.size();
        }
        const std::string pair = assetPath.substr(position, end - position);
        if (pair.rfind("v=", 0) == 0) {
            return pair.substr(2);
        }
        position = end + 1;
    }
    return {};
}

bool HasExplicitVersionPin(const std::string& assetPath)
{
    std::string value = VersionSelectorValue(assetPath);
    if (!value.empty() && value[0] == 'v') {
        value = value.substr(1);
    }
    if (value.empty()) {
        return false;
    }
    for (const char ch : value) {
        if (ch < '0' || ch > '9') {
            return false;
        }
    }
    return true;
}

// WIP heads move on every registered write, so wip resolutions must never be
// cached (schema v8 cache policy).
bool HasWipSelector(const std::string& assetPath)
{
    return VersionSelectorValue(assetPath) == "wip";
}

long CacheTtlSeconds()
{
    const std::string value = GetEnv("ADS_RESOLVER_CACHE_TTL_SECONDS", "30");
    try {
        return std::stol(value);
    } catch (...) {
        return 30;
    }
}

// The two resolve caches (server and CLI) live at file scope so that
// _RefreshContext can drop them when the host asks for a refresh.
struct ResolveCache
{
    std::mutex mutex;
    std::unordered_map<std::string, ResolveCacheEntry> entries;
};

ResolveCache& ServerResolveCache()
{
    static ResolveCache cache;
    return cache;
}

ResolveCache& CliResolveCache()
{
    static ResolveCache cache;
    return cache;
}

void ClearResolveCaches()
{
    for (ResolveCache* cache : {&ServerResolveCache(), &CliResolveCache()}) {
        std::lock_guard<std::mutex> lock(cache->mutex);
        cache->entries.clear();
    }
}

bool LookupResolveCache(
    std::mutex& mutex,
    std::unordered_map<std::string, ResolveCacheEntry>& cache,
    const std::string& key,
    std::string* location)
{
    std::lock_guard<std::mutex> lock(mutex);
    const auto found = cache.find(key);
    if (found == cache.end()) {
        return false;
    }
    if (!found->second.permanent && std::chrono::steady_clock::now() >= found->second.expiry) {
        cache.erase(found);
        return false;
    }
    *location = found->second.location;
    return true;
}

void StoreResolveCache(
    std::mutex& mutex,
    std::unordered_map<std::string, ResolveCacheEntry>& cache,
    const std::string& key,
    const std::string& location,
    bool permanent)
{
    ResolveCacheEntry entry;
    entry.location = location;
    entry.permanent = permanent;
    if (!permanent) {
        const long ttl = CacheTtlSeconds();
        if (ttl <= 0) {
            return;
        }
        entry.expiry = std::chrono::steady_clock::now() + std::chrono::seconds(ttl);
    }
    std::lock_guard<std::mutex> lock(mutex);
    cache[key] = std::move(entry);
}

std::string ResolveWithAdsServer(const std::string& assetPath)
{
    const std::string server = TrimTrailingSlashes(GetEnv("ADS_RESOLVER_SERVER"));
    if (server.empty()) {
        return {};
    }

    const std::string profile = GetEnv("ADS_RESOLVER_PROFILE", "main");
    const std::string mode = GetEnv("ADS_RESOLVER_MODE", "remote");
    const std::string token = GetEnv(
        "ADS_RESOLVER_API_TOKEN",
        GetEnv("ADS_RESOLVER_HTTP_BEARER_TOKEN", GetEnv("ADS_WEB_TOKEN")));
    const std::string timeoutSeconds = GetEnv("ADS_RESOLVER_HTTP_TIMEOUT_SECONDS", "30");
    const std::string normalizedAssetPath = NormalizeAdsUri(StripSdfFormatArgs(assetPath));

    const std::string cacheKey =
        "server\n" + server + "\n" + profile + "\n" + mode + "\n" + normalizedAssetPath;
    ResolveCache& cache = ServerResolveCache();
    const bool wip = HasWipSelector(normalizedAssetPath);
    std::string cached;
    if (!wip && LookupResolveCache(cache.mutex, cache.entries, cacheKey, &cached)) {
        return cached;
    }

    const std::string url = server + "/api/resolve?profile=" + PercentEncode(profile)
        + "&asset_path=" + PercentEncode(normalizedAssetPath) + "&mode=" + PercentEncode(mode);

    if (DebugEnabled()) {
        LogResolverMessage("ADS Resolver API resolve `" + normalizedAssetPath + "` via `" + url + "`");
    }

    const std::string response = HttpGetText(url, token, timeoutSeconds);
    const std::string location = JsonStringField(response, "location");
    if (DebugEnabled()) {
        LogResolverMessage("ADS Resolver API resolved `" + normalizedAssetPath + "` -> `" + location + "`");
    }
    if (location.empty()) {
        WarnOnce(
            "server-resolve\n" + server,
            "ADS Resolver: failed to resolve `" + normalizedAssetPath + "` from server `" + server
                + "` profile `" + profile + "`: "
                + (response.empty()
                       ? "no response (server down, connection refused, timeout, or HTTP error)"
                       : "response has no `location` field (malformed JSON or server-side error)"));
    }
    if (!location.empty() && !wip) {
        StoreResolveCache(
            cache.mutex,
            cache.entries,
            cacheKey,
            location,
            HasExplicitVersionPin(normalizedAssetPath));
    }
    return location;
}

std::string ObjectBearerToken()
{
    return GetEnv(
        "ADS_RESOLVER_OBJECT_BEARER_TOKEN",
        GetEnv(
            "ADS_RESOLVER_HTTP_BEARER_TOKEN",
            GetEnv("ADS_RESOLVER_API_TOKEN", GetEnv("ADS_WEB_TOKEN"))));
}

std::shared_ptr<ArAsset> OpenRemoteAsset(const std::string& url)
{
    const std::string timeoutSeconds = GetEnv("ADS_RESOLVER_HTTP_TIMEOUT_SECONDS", "30");
    if (DebugEnabled()) {
        LogResolverMessage("ADS Resolver remote asset download `" + url + "`");
    }

    std::vector<char> bytes;
    if (!HttpGetBytes(url, ObjectBearerToken(), timeoutSeconds, &bytes)) {
        WarnOnce(
            "download\n" + UrlOrigin(url),
            "ADS Resolver: failed to download remote asset `" + url
                + "`; the layer will fail to open");
        return {};
    }

    auto storage = std::make_shared<std::vector<char>>(std::move(bytes));
    std::shared_ptr<const char> buffer(storage, storage->empty() ? nullptr : storage->data());
    return ArInMemoryAsset::FromBuffer(std::move(buffer), storage->size());
}

// FIPS 180-4 SHA-256, compact and standalone so downloaded blobs can be
// verified against the content hash in the object URL without adding a link
// dependency. Single-shot over an in-memory buffer; downloads dominate the
// cost. Returns lowercase hex.
std::string Sha256Hex(const unsigned char* data, size_t size)
{
    static constexpr uint32_t kRoundConstants[64] = {
        0x428a2f98u, 0x71374491u, 0xb5c0fbcfu, 0xe9b5dba5u,
        0x3956c25bu, 0x59f111f1u, 0x923f82a4u, 0xab1c5ed5u,
        0xd807aa98u, 0x12835b01u, 0x243185beu, 0x550c7dc3u,
        0x72be5d74u, 0x80deb1feu, 0x9bdc06a7u, 0xc19bf174u,
        0xe49b69c1u, 0xefbe4786u, 0x0fc19dc6u, 0x240ca1ccu,
        0x2de92c6fu, 0x4a7484aau, 0x5cb0a9dcu, 0x76f988dau,
        0x983e5152u, 0xa831c66du, 0xb00327c8u, 0xbf597fc7u,
        0xc6e00bf3u, 0xd5a79147u, 0x06ca6351u, 0x14292967u,
        0x27b70a85u, 0x2e1b2138u, 0x4d2c6dfcu, 0x53380d13u,
        0x650a7354u, 0x766a0abbu, 0x81c2c92eu, 0x92722c85u,
        0xa2bfe8a1u, 0xa81a664bu, 0xc24b8b70u, 0xc76c51a3u,
        0xd192e819u, 0xd6990624u, 0xf40e3585u, 0x106aa070u,
        0x19a4c116u, 0x1e376c08u, 0x2748774cu, 0x34b0bcb5u,
        0x391c0cb3u, 0x4ed8aa4au, 0x5b9cca4fu, 0x682e6ff3u,
        0x748f82eeu, 0x78a5636fu, 0x84c87814u, 0x8cc70208u,
        0x90befffau, 0xa4506cebu, 0xbef9a3f7u, 0xc67178f2u,
    };

    uint32_t state[8] = {
        0x6a09e667u, 0xbb67ae85u, 0x3c6ef372u, 0xa54ff53au,
        0x510e527fu, 0x9b05688cu, 0x1f83d9abu, 0x5be0cd19u,
    };

    const auto rotr = [](uint32_t value, int bits) -> uint32_t {
        return (value >> bits) | (value << (32 - bits));
    };
    const auto processBlock = [&](const unsigned char* block) {
        uint32_t w[64];
        for (int i = 0; i < 16; ++i) {
            w[i] = (uint32_t(block[i * 4]) << 24) | (uint32_t(block[i * 4 + 1]) << 16)
                | (uint32_t(block[i * 4 + 2]) << 8) | uint32_t(block[i * 4 + 3]);
        }
        for (int i = 16; i < 64; ++i) {
            const uint32_t s0 = rotr(w[i - 15], 7) ^ rotr(w[i - 15], 18) ^ (w[i - 15] >> 3);
            const uint32_t s1 = rotr(w[i - 2], 17) ^ rotr(w[i - 2], 19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16] + s0 + w[i - 7] + s1;
        }
        uint32_t a = state[0], b = state[1], c = state[2], d = state[3];
        uint32_t e = state[4], f = state[5], g = state[6], h = state[7];
        for (int i = 0; i < 64; ++i) {
            const uint32_t sum1 = rotr(e, 6) ^ rotr(e, 11) ^ rotr(e, 25);
            const uint32_t choose = (e & f) ^ (~e & g);
            const uint32_t temp1 = h + sum1 + choose + kRoundConstants[i] + w[i];
            const uint32_t sum0 = rotr(a, 2) ^ rotr(a, 13) ^ rotr(a, 22);
            const uint32_t majority = (a & b) ^ (a & c) ^ (b & c);
            const uint32_t temp2 = sum0 + majority;
            h = g;
            g = f;
            f = e;
            e = d + temp1;
            d = c;
            c = b;
            b = a;
            a = temp1 + temp2;
        }
        state[0] += a;
        state[1] += b;
        state[2] += c;
        state[3] += d;
        state[4] += e;
        state[5] += f;
        state[6] += g;
        state[7] += h;
    };

    size_t offset = 0;
    for (; offset + 64 <= size; offset += 64) {
        processBlock(data + offset);
    }

    // Final block(s): remaining bytes, 0x80, zero pad, 64-bit big-endian bit
    // count. remaining < 56 fits one block, otherwise two.
    unsigned char tail[128] = {};
    const size_t remaining = size - offset;
    if (remaining > 0) {
        std::memcpy(tail, data + offset, remaining);
    }
    tail[remaining] = 0x80;
    const size_t tailLength = remaining < 56 ? 64 : 128;
    const uint64_t bitCount = static_cast<uint64_t>(size) * 8;
    for (int i = 0; i < 8; ++i) {
        tail[tailLength - 8 + i] = static_cast<unsigned char>(bitCount >> (56 - 8 * i));
    }
    processBlock(tail);
    if (tailLength == 128) {
        processBlock(tail + 64);
    }

    static const char* kHexDigits = "0123456789abcdef";
    std::string digest;
    digest.reserve(64);
    for (const uint32_t word : state) {
        for (int shift = 28; shift >= 0; shift -= 4) {
            digest.push_back(kHexDigits[(word >> shift) & 0xf]);
        }
    }
    return digest;
}

bool IsHexDigit(char ch)
{
    return (ch >= '0' && ch <= '9') || (ch >= 'a' && ch <= 'f') || (ch >= 'A' && ch <= 'F');
}

// The object URL path embeds the content hash:
// .../objects/sha256/<2-hex-prefix>/<64-hex>[.<ext>]. Exactly 64 hex chars
// are required; anything else means the URL is not a content-addressed object
// endpoint and the caller falls back to the in-memory download.
std::string ParseSha256FromObjectUrl(const std::string& url)
{
    const std::string path = StripQuery(url);
    const std::string marker = "objects/sha256/";
    const size_t markerPos = path.find(marker);
    if (markerPos == std::string::npos) {
        return {};
    }
    size_t pos = markerPos + marker.size();
    if (pos + 3 > path.size() || !IsHexDigit(path[pos]) || !IsHexDigit(path[pos + 1])
        || path[pos + 2] != '/') {
        return {};
    }
    pos += 3;
    size_t end = pos;
    while (end < path.size() && IsHexDigit(path[end])) {
        ++end;
    }
    if (end - pos != 64) {
        return {};
    }
    std::string hash = path.substr(pos, 64);
    std::transform(hash.begin(), hash.end(), hash.begin(), [](unsigned char ch) {
        return static_cast<char>(std::tolower(ch));
    });
    return hash;
}

// Blob cache root: ADS_RESOLVER_CACHE_DIR, else the CLI workspace blob cache
// (<workspace>/.ads-cache) so `ads cache gc` manages both, else a per-user
// location. The sha256/ leaf is appended so every layout matches the CLI's
// flat blob cache: sha256/<2-hex-prefix>/<hash>.<ext>.
std::string BlobCacheRoot()
{
    std::string base = GetEnv("ADS_RESOLVER_CACHE_DIR");
    if (base.empty()) {
        const std::string workspace = GetEnv("ADS_RESOLVER_WORKSPACE");
        if (!workspace.empty()) {
            base = workspace + "/.ads-cache";
        }
    }
    if (base.empty()) {
#if defined(_WIN32)
        const std::string localAppData = GetEnv("LOCALAPPDATA");
        if (!localAppData.empty()) {
            base = localAppData + "/ads/resolver-cache";
        }
#else
        const std::string xdgCacheHome = GetEnv("XDG_CACHE_HOME");
        if (!xdgCacheHome.empty()) {
            base = xdgCacheHome + "/ads/resolver-cache";
        } else {
            const std::string home = GetEnv("HOME");
            if (!home.empty()) {
                base = home + "/.cache/ads/resolver-cache";
            }
        }
#endif
    }
    if (base.empty()) {
        return {};
    }
    return NormalizeSlashes(base) + "/sha256";
}

std::string AssetPathExtension(const std::string& assetPath)
{
    const std::string normalized = StripQuery(StripSdfFormatArgs(NormalizeSlashes(assetPath)));
    const size_t slashPos = normalized.find_last_of('/');
    const size_t dotPos = normalized.find_last_of('.');
    if (dotPos == std::string::npos || (slashPos != std::string::npos && dotPos < slashPos)) {
        return {};
    }
    return normalized.substr(dotPos + 1);
}

// Mirrors the CLI's VIEW_EXTENSIONS (src/lib.rs): formats that may hold
// relative references to sibling files. The publish policy only forces
// ads:// for cross-asset references, so intra-version relative refs are a
// supported shape — these formats must keep resolving to their ads:// URI
// (see _Resolve), because USD anchors relative refs against the layer's
// resolved path and only an ads:// anchor re-anchors them to ads://
// identifiers instead of nonexistent flat-blob-cache siblings.
bool IsComposingAssetPath(const std::string& assetPath)
{
    std::string extension = AssetPathExtension(assetPath);
    std::transform(extension.begin(), extension.end(), extension.begin(), [](unsigned char ch) {
        return static_cast<char>(std::tolower(ch));
    });
    return extension == "usd" || extension == "usda" || extension == "usdc"
        || extension == "usdz" || extension == "mtlx";
}

unsigned long CurrentProcessId()
{
#if defined(_WIN32)
    return GetCurrentProcessId();
#else
    return static_cast<unsigned long>(getpid());
#endif
}

// In-flight blob download deduplication: N composition threads asking for the
// same object should trigger one download; the rest wait, then take the cache
// hit. The mutex only guards the in-flight set — it is never held across
// network or file I/O, so this cannot deadlock against the download itself.
struct BlobDownloadGate
{
    std::mutex mutex;
    std::condition_variable condition;
    std::unordered_set<std::string> inFlight;
};

BlobDownloadGate& TheBlobDownloadGate()
{
    static BlobDownloadGate gate;
    return gate;
}

// Materializes a remote object URL into the content-addressed blob cache and
// returns the cached file path (empty means: keep the in-memory fallback).
// Blobs are immutable — they are addressed by their sha256 — so this cache is
// permanent for every version selector including wip; the resolve cache
// policy governs URI -> location staleness, not blob content. The extension
// comes from the ads:// URI leaf, matching the CLI blob cache layout.
std::string FetchRemoteObjectToCache(
    const std::string& url,
    const std::string& adsUri,
    bool downloadOnMiss)
{
    const std::string hash = ParseSha256FromObjectUrl(url);
    if (hash.empty()) {
        if (downloadOnMiss) {
            WarnOnce(
                "blob-cache-hash\n" + UrlOrigin(url),
                "ADS Resolver: object URL `" + url
                    + "` has no parseable sha256; falling back to in-memory download");
        }
        return {};
    }

    const std::string root = BlobCacheRoot();
    if (root.empty()) {
        if (downloadOnMiss) {
            WarnOnce(
                "blob-cache-root",
                "ADS Resolver: no blob cache directory available (set "
                "ADS_RESOLVER_CACHE_DIR or ADS_RESOLVER_WORKSPACE); falling back to "
                "in-memory download");
        }
        return {};
    }

    const std::string extension = AssetPathExtension(adsUri);
    const std::string fileName = extension.empty() ? hash : hash + "." + extension;
    const std::string finalPath = root + "/" + hash.substr(0, 2) + "/" + fileName;

    std::error_code fsError;
    if (std::filesystem::exists(ToFsPath(finalPath), fsError)) {
        return finalPath;
    }
    if (!downloadOnMiss) {
        return {};
    }

    BlobDownloadGate& gate = TheBlobDownloadGate();
    {
        std::unique_lock<std::mutex> lock(gate.mutex);
        while (gate.inFlight.count(hash) > 0) {
            gate.condition.wait(lock);
        }
        gate.inFlight.insert(hash);
    }
    struct GateRelease
    {
        BlobDownloadGate& gate;
        const std::string& hash;
        ~GateRelease()
        {
            {
                std::lock_guard<std::mutex> lock(gate.mutex);
                gate.inFlight.erase(hash);
            }
            gate.condition.notify_all();
        }
    } gateRelease { gate, hash };

    // Another thread may have finished this download while we waited.
    if (std::filesystem::exists(ToFsPath(finalPath), fsError)) {
        return finalPath;
    }

    const std::string timeoutSeconds = GetEnv("ADS_RESOLVER_HTTP_TIMEOUT_SECONDS", "30");
    if (DebugEnabled()) {
        LogResolverMessage(
            "ADS Resolver blob cache download `" + url + "` -> `" + finalPath + "`");
    }
    std::vector<char> bytes;
    if (!HttpGetBytes(url, ObjectBearerToken(), timeoutSeconds, &bytes)) {
        WarnOnce(
            "download\n" + UrlOrigin(url),
            "ADS Resolver: failed to download remote asset `" + url
                + "`; the layer will fail to open");
        return {};
    }

    // Verify content before anything lands at the final path: a truncated or
    // tampered download must never become a cache hit.
    const std::string actualHash = Sha256Hex(
        reinterpret_cast<const unsigned char*>(bytes.data()), bytes.size());
    if (actualHash != hash) {
        WarnOnce(
            "blob-cache-verify\n" + url,
            "ADS Resolver: downloaded object `" + url + "` hashed to " + actualHash
                + " but the URL names " + hash
                + "; not caching (falling back to in-memory download)");
        return {};
    }

    std::filesystem::create_directories(ToFsPath(finalPath).parent_path(), fsError);

    static std::atomic<unsigned long> tempSerial { 0 };
    std::ostringstream tempName;
    tempName << finalPath << ".tmp." << CurrentProcessId() << "." << tempSerial.fetch_add(1);
    const std::string tempPath = tempName.str();

    bool written = false;
    {
        std::ofstream stream(ToFsPath(tempPath), std::ios::binary | std::ios::trunc);
        if (stream) {
            if (!bytes.empty()) {
                stream.write(bytes.data(), static_cast<std::streamsize>(bytes.size()));
            }
            stream.close();
            written = !stream.fail();
        }
    }
    if (!written) {
        WarnOnce(
            "blob-cache-write\n" + root,
            "ADS Resolver: cannot write blob cache files under `" + root
                + "`; falling back to in-memory download");
        std::filesystem::remove(ToFsPath(tempPath), fsError);
        return {};
    }

    std::filesystem::rename(ToFsPath(tempPath), ToFsPath(finalPath), fsError);
    if (fsError) {
        // Rename onto an existing file fails on Windows: a concurrent writer
        // (another process; in-process racers are deduplicated above) won, and
        // its file has identical content — use it.
        std::filesystem::remove(ToFsPath(tempPath), fsError);
        std::error_code raceError;
        if (std::filesystem::exists(ToFsPath(finalPath), raceError)) {
            return finalPath;
        }
        WarnOnce(
            "blob-cache-write\n" + root,
            "ADS Resolver: cannot finalize blob cache files under `" + root
                + "`; falling back to in-memory download");
        return {};
    }
    return finalPath;
}

std::string ResolveWithAdsCli(const std::string& assetPath)
{
    const std::string executable = GetEnv("ADS_RESOLVER_EXECUTABLE", "ads");
    const std::string store = GetEnv("ADS_RESOLVER_STORE");
    const std::string workspace = GetEnv("ADS_RESOLVER_WORKSPACE");
    const std::string mode = GetEnv("ADS_RESOLVER_MODE", "local");
    const std::string remoteBaseUrl = GetEnv("ADS_RESOLVER_REMOTE_BASE_URL");
    const std::string cliAssetPath = NormalizeAdsUri(StripSdfFormatArgs(assetPath));

    if (store.empty()) {
        WarnOnce(
            "cli-config",
            "ADS Resolver: ADS_RESOLVER_STORE is not set; cannot resolve `" + cliAssetPath
                + "` with the `" + executable + "` CLI");
        return {};
    }

    const std::string cacheKey = "cli\n" + executable + "\n" + store + "\n" + workspace + "\n" + mode
        + "\n" + remoteBaseUrl + "\n" + cliAssetPath;
    ResolveCache& cache = CliResolveCache();
    const bool wip = HasWipSelector(cliAssetPath);
    std::string cached;
    if (!wip && LookupResolveCache(cache.mutex, cache.entries, cacheKey, &cached)) {
        return cached;
    }

    std::vector<std::string> args {
        executable,
        "resolve",
        "--store",
        store,
    };
    if (!workspace.empty()) {
        args.push_back("--workspace");
        args.push_back(workspace);
    }
    args.push_back("--mode");
    args.push_back(mode);
    if (!remoteBaseUrl.empty()) {
        args.push_back("--remote-base-url");
        args.push_back(remoteBaseUrl);
    }
    args.push_back(cliAssetPath);

    if (DebugEnabled()) {
        LogResolverMessage("ADS Resolver command: " + JoinProcessCommandLine(args));
    }

    const std::string resolved = ReadCommandStdout(args);
    if (DebugEnabled()) {
        LogResolverMessage("ADS Resolver resolved `" + assetPath + "` -> `" + resolved + "`");
    }
    if (resolved.empty()) {
        WarnOnce(
            "cli-resolve\n" + executable + "\n" + store,
            "ADS Resolver: CLI failed to resolve `" + cliAssetPath + "` (command: "
                + JoinProcessCommandLine(args) + ")");
    }
    if (!resolved.empty() && !wip) {
        StoreResolveCache(
            cache.mutex,
            cache.entries,
            cacheKey,
            resolved,
            HasExplicitVersionPin(cliAssetPath));
    }
    return resolved;
}

std::string ResolveAssetPath(const std::string& assetPath)
{
    // Single chokepoint for the scheme policy (see StartsWithAdsScheme): both
    // _Resolve and _OpenAsset funnel through here, so malformed ads URIs like
    // `ads:foo` (no authority slashes) are rejected consistently instead of
    // being passed to the server or CLI as a guess.
    if (!IsWellFormedAdsUri(NormalizeSlashes(assetPath))) {
        WarnOnce(
            "malformed-uri\n" + assetPath,
            "ADS Resolver: malformed ads URI `" + assetPath
                + "` (expected ads://<path>); refusing to resolve");
        return {};
    }

    const std::string serverResolved = ResolveWithAdsServer(assetPath);
    if (!serverResolved.empty()) {
        return serverResolved;
    }
    // Only a degraded fallback when a server was configured; with no server
    // the CLI is the primary resolution path and nothing is stale.
    const std::string server = TrimTrailingSlashes(GetEnv("ADS_RESOLVER_SERVER"));
    if (!server.empty()) {
        WarnOnce(
            "cli-fallback\n" + server,
            "ADS Resolver: server resolve via `" + server + "` failed for `" + assetPath
                + "`; falling back to local CLI resolution, results may be stale local content");
    }
    return ResolveWithAdsCli(assetPath);
}

} // namespace

class AdsResolver final : public ArResolver
{
public:
    AdsResolver() = default;
    ~AdsResolver() override = default;

protected:
    std::string _CreateIdentifier(
        const std::string& assetPath,
        const ArResolvedPath& anchorAssetPath) const override
    {
        if (StartsWithAdsScheme(assetPath)) {
            return NormalizeSlashes(assetPath);
        }
        if (!anchorAssetPath.empty() && !ToFsPath(assetPath).is_absolute()) {
            const std::string adsIdentifier = CreateAdsRelativeIdentifier(assetPath, anchorAssetPath);
            if (!adsIdentifier.empty()) {
                return adsIdentifier;
            }
            const std::filesystem::path anchor = ToFsPath(anchorAssetPath.GetPathString());
            return NormalizeSlashes(
                FsPathToUtf8((anchor.parent_path() / ToFsPath(assetPath)).lexically_normal()));
        }
        return NormalizeSlashes(FsPathToUtf8(ToFsPath(assetPath).lexically_normal()));
    }

    std::string _CreateIdentifierForNewAsset(
        const std::string& assetPath,
        const ArResolvedPath& anchorAssetPath) const override
    {
        return _CreateIdentifier(assetPath, anchorAssetPath);
    }

    ArResolvedPath _Resolve(const std::string& assetPath) const override
    {
        if (!StartsWithAdsScheme(assetPath)) {
            return {};
        }
        const std::string resolved = NormalizeSlashes(ResolveAssetPath(assetPath));
        if (resolved.empty()) {
            return {};
        }
        if (StartsWithHttpScheme(resolved)) {
            // Remote objects are materialized into the content-addressed blob
            // cache so opens read from disk and unchanged content is never
            // re-downloaded. Only leaf content resolves to the cache file
            // itself; composing formats keep their ads:// identity so
            // relative references anchored against the resolved path
            // re-anchor to ads:// URIs (a flat cache path would send them to
            // nonexistent cache-file siblings), while their bytes still open
            // from the cache warmed here.
            const std::string cached =
                FetchRemoteObjectToCache(resolved, assetPath, /*downloadOnMiss=*/true);
            if (!cached.empty() && !IsComposingAssetPath(assetPath)) {
                return ArResolvedPath(cached);
            }
            // Composing formats and cache-less fallbacks route back through
            // _OpenAsset: blob cache hit when available, otherwise the
            // pre-cache behavior of streaming into an ArInMemoryAsset.
            return ArResolvedPath(NormalizeSlashes(assetPath));
        }
        return ArResolvedPath(resolved);
    }

    ArResolvedPath _ResolveForNewAsset(const std::string& assetPath) const override
    {
        // ADS URI writes are intentionally disabled for the first resolver phase.
        // Artists should create/edit explicit workspace version folders and publish
        // through `ads add`.
        return StartsWithAdsScheme(assetPath) ? ArResolvedPath() : ArResolvedPath(assetPath);
    }

    bool _IsContextDependentPath(const std::string& assetPath) const override
    {
        return StartsWithAdsScheme(assetPath);
    }

    // Hosts call ArResolver::RefreshContext() (e.g. from a pipeline menu or
    // shelf tool) to pick up server-side current/latest moves without waiting
    // for the TTL or restarting. Drop both resolve caches, then tell every
    // listener (usdview, Solaris, ...) that previously resolved paths may now
    // resolve differently so open stages re-resolve.
    void _RefreshContext(const ArResolverContext& /*context*/) override
    {
        ClearResolveCaches();
        if (DebugEnabled()) {
            LogResolverMessage("ADS Resolver caches cleared by RefreshContext");
        }
        ArNotice::ResolverChanged().Send();
    }

    std::string _GetExtension(const std::string& assetPath) const override
    {
        return AssetPathExtension(assetPath);
    }

    ArTimestamp _GetModificationTimestamp(
        const std::string& assetPath,
        const ArResolvedPath& resolvedPath) const override
    {
        if (resolvedPath.empty()) {
            return {};
        }
        if (DebugEnabled()) {
            LogResolverMessage(
                "ADS Resolver timestamp `" + assetPath + "` resolved `"
                + resolvedPath.GetPathString() + "`");
        }
        if (StartsWithAdsScheme(resolvedPath.GetPathString())) {
            // Composing remote layers keep their ads:// identity (see
            // _Resolve) but their bytes live in the immutable blob cache.
            // The cache file changes exactly when the URI resolves to
            // different content, so its timestamp is a faithful modification
            // stamp and keeps SdfLayer::Reload from refetching unchanged
            // layers.
            const std::string resolved =
                NormalizeSlashes(ResolveAssetPath(resolvedPath.GetPathString()));
            if (StartsWithHttpScheme(resolved)) {
                const std::string cached = FetchRemoteObjectToCache(
                    resolved, resolvedPath.GetPathString(), /*downloadOnMiss=*/false);
                if (!cached.empty()) {
                    return ArFilesystemAsset::GetModificationTimestamp(ArResolvedPath(cached));
                }
            }
            return {};
        }
        if (StartsWithHttpScheme(resolvedPath.GetPathString())) {
            return {};
        }
        return ArFilesystemAsset::GetModificationTimestamp(resolvedPath);
    }

    std::shared_ptr<ArAsset> _OpenAsset(const ArResolvedPath& resolvedPath) const override
    {
        if (resolvedPath.empty()) {
            return {};
        }
        if (DebugEnabled()) {
            LogResolverMessage("ADS Resolver open asset `" + resolvedPath.GetPathString() + "`");
        }
        if (StartsWithAdsScheme(resolvedPath.GetPathString())) {
            const std::string resolved = NormalizeSlashes(ResolveAssetPath(resolvedPath.GetPathString()));
            if (StartsWithHttpScheme(resolved)) {
                // Composing formats resolve to their ads:// URI but _Resolve
                // already warmed the blob cache, so this probe (cheap) is
                // normally a hit; a miss means _Resolve could not use the
                // cache at all, so do not retry the download machinery per
                // open.
                const std::string cached = FetchRemoteObjectToCache(
                    resolved, resolvedPath.GetPathString(), /*downloadOnMiss=*/false);
                if (!cached.empty()) {
                    return ArFilesystemAsset::Open(ArResolvedPath(cached));
                }
                return OpenRemoteAsset(resolved);
            }
            if (!resolved.empty()) {
                return ArFilesystemAsset::Open(ArResolvedPath(resolved));
            }
            return {};
        }
        if (StartsWithHttpScheme(resolvedPath.GetPathString())) {
            return OpenRemoteAsset(resolvedPath.GetPathString());
        }
        auto asset = ArFilesystemAsset::Open(resolvedPath);
        if (!asset && DebugEnabled()) {
            LogResolverMessage("ADS Resolver failed to open `" + resolvedPath.GetPathString() + "`");
        }
        return asset;
    }

    bool _CanWriteAssetToPath(
        const ArResolvedPath& resolvedPath,
        std::string* whyNot) const override
    {
        if (whyNot) {
            *whyNot = "ADS resolver is read-only; create/edit a workspace version folder and publish with ads add";
        }
        return false;
    }

    std::shared_ptr<ArWritableAsset> _OpenAssetForWrite(
        const ArResolvedPath& resolvedPath,
        WriteMode writeMode) const override
    {
        return {};
    }
};

AR_DEFINE_RESOLVER(AdsResolver, ArResolver);

// Direct refresh entry point for pipeline tooling (ads.usd_refresh).
//
// ArResolver::RefreshContext() only reaches the primary resolver in the USD
// builds shipped with current Houdini releases — URI resolvers like this one
// never see _RefreshContext. Pipeline code therefore calls this exported
// function (via ctypes on the already-loaded plugin module) to drop the
// resolve caches and broadcast ArNotice::ResolverChanged so open stages
// re-resolve ads:// paths.
extern "C"
#if defined(_WIN32)
__declspec(dllexport)
#endif
void AdsResolverRefreshCaches()
{
    ClearResolveCaches();
    if (DebugEnabled()) {
        LogResolverMessage("ADS Resolver caches cleared by AdsResolverRefreshCaches");
    }
    ArNotice::ResolverChanged().Send();
}

PXR_NAMESPACE_CLOSE_SCOPE
