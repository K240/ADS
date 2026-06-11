#include "pxr/pxr.h"
#include "pxr/usd/ar/defineResolver.h"
#include "pxr/usd/ar/filesystemAsset.h"
#include "pxr/usd/ar/filesystemWritableAsset.h"
#include "pxr/usd/ar/inMemoryAsset.h"
#include "pxr/usd/ar/notice.h"
#include "pxr/usd/ar/resolvedPath.h"
#include "pxr/usd/ar/resolver.h"

#include <algorithm>
#include <array>
#include <cctype>
#include <chrono>
#include <cstdio>
#include <cstdlib>
#include <filesystem>
#include <fstream>
#include <iostream>
#include <memory>
#include <mutex>
#include <sstream>
#include <string>
#include <unordered_map>
#include <vector>

#if defined(_WIN32)
#ifndef NOMINMAX
#define NOMINMAX
#endif
#include <windows.h>
#include <winhttp.h>
#pragma comment(lib, "winhttp.lib")
#endif

PXR_NAMESPACE_OPEN_SCOPE

namespace {

constexpr const char* kAdsScheme = "ads:";

bool StartsWithAdsScheme(const std::string& value)
{
    return value.rfind(kAdsScheme, 0) == 0;
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

std::string GetEnv(const char* name, const std::string& fallback = "")
{
    if (const char* value = std::getenv(name)) {
        return value;
    }
    return fallback;
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
    std::ofstream stream(logFile, std::ios::app);
    if (stream) {
        stream << message << "\n";
    }
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
    const std::string prefix = "ads://";
    if (uri.rfind(prefix, 0) != 0) {
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
#endif

bool ReadCommandBytes(const std::vector<std::string>& args, std::vector<char>* output)
{
    std::array<char, 64 * 1024> buffer {};

#if defined(_WIN32)
    SECURITY_ATTRIBUTES securityAttributes {};
    securityAttributes.nLength = sizeof(securityAttributes);
    securityAttributes.bInheritHandle = TRUE;

    HANDLE readPipeRaw = nullptr;
    HANDLE writePipeRaw = nullptr;
    if (!CreatePipe(&readPipeRaw, &writePipeRaw, &securityAttributes, 0)) {
        return false;
    }
    ScopedHandle readPipe(readPipeRaw);
    ScopedHandle writePipe(writePipeRaw);
    SetHandleInformation(readPipe.get(), HANDLE_FLAG_INHERIT, 0);

    ScopedHandle nul(CreateFileW(
        L"NUL",
        GENERIC_WRITE,
        FILE_SHARE_READ | FILE_SHARE_WRITE,
        &securityAttributes,
        OPEN_EXISTING,
        FILE_ATTRIBUTE_NORMAL,
        nullptr));

    STARTUPINFOW startupInfo {};
    startupInfo.cb = sizeof(startupInfo);
    startupInfo.dwFlags = STARTF_USESTDHANDLES;
    startupInfo.hStdInput = GetStdHandle(STD_INPUT_HANDLE);
    startupInfo.hStdOutput = writePipe.get();
    startupInfo.hStdError = nul.get() == INVALID_HANDLE_VALUE ? writePipe.get() : nul.get();

    PROCESS_INFORMATION processInfo {};
    std::wstring commandLine = Utf8ToWide(JoinProcessCommandLine(args));
    if (commandLine.empty()) {
        return false;
    }
    std::vector<wchar_t> mutableCommandLine(commandLine.begin(), commandLine.end());
    mutableCommandLine.push_back(L'\0');

    const BOOL created = CreateProcessW(
        nullptr,
        mutableCommandLine.data(),
        nullptr,
        nullptr,
        TRUE,
        CREATE_NO_WINDOW,
        nullptr,
        nullptr,
        &startupInfo,
        &processInfo);

    writePipe.reset();
    nul.reset();

    if (!created) {
        return false;
    }

    ScopedHandle process(processInfo.hProcess);
    ScopedHandle thread(processInfo.hThread);

    output->clear();
    while (true) {
        DWORD bytesRead = 0;
        const BOOL read = ReadFile(
            readPipe.get(),
            buffer.data(),
            static_cast<DWORD>(buffer.size()),
            &bytesRead,
            nullptr);
        if (!read || bytesRead == 0) {
            break;
        }
        output->insert(output->end(), buffer.data(), buffer.data() + bytesRead);
    }

    WaitForSingleObject(process.get(), INFINITE);
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
#if defined(_WIN32)
                _pclose(pipe);
#else
                pclose(pipe);
#endif
                output->clear();
                return false;
            }
        }
    }

#if defined(_WIN32)
    const int status = _pclose(pipe);
#else
    const int status = pclose(pipe);
#endif
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
                LogResolverMessage(
                    "ADS Resolver refused remote object: Content-Length "
                        + std::to_string(announced) + " exceeds ADS_RESOLVER_MAX_DOWNLOAD_MB",
                    true);
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
            LogResolverMessage(
                "ADS Resolver aborted remote object download: size exceeds "
                "ADS_RESOLVER_MAX_DOWNLOAD_MB",
                true);
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
        LogResolverMessage(
            "ADS Resolver API failed to resolve `" + normalizedAssetPath + "` from server `" + server
                + "` profile `" + profile + "`",
            true);
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

std::shared_ptr<ArAsset> OpenRemoteAsset(const std::string& url)
{
    const std::string bearerToken = GetEnv(
        "ADS_RESOLVER_OBJECT_BEARER_TOKEN",
        GetEnv(
            "ADS_RESOLVER_HTTP_BEARER_TOKEN",
            GetEnv("ADS_RESOLVER_API_TOKEN", GetEnv("ADS_WEB_TOKEN"))));
    const std::string timeoutSeconds = GetEnv("ADS_RESOLVER_HTTP_TIMEOUT_SECONDS", "30");
    if (DebugEnabled()) {
        LogResolverMessage("ADS Resolver remote asset download `" + url + "`");
    }

    std::vector<char> bytes;
    if (!HttpGetBytes(url, bearerToken, timeoutSeconds, &bytes)) {
        LogResolverMessage("ADS Resolver failed to download remote asset `" + url + "`", true);
        return {};
    }

    auto storage = std::make_shared<std::vector<char>>(std::move(bytes));
    std::shared_ptr<const char> buffer(storage, storage->empty() ? nullptr : storage->data());
    return ArInMemoryAsset::FromBuffer(std::move(buffer), storage->size());
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
        LogResolverMessage("ADS Resolver: ADS_RESOLVER_STORE is not set", true);
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
    const std::string serverResolved = ResolveWithAdsServer(assetPath);
    if (!serverResolved.empty()) {
        return serverResolved;
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
        if (!anchorAssetPath.empty() && !std::filesystem::path(assetPath).is_absolute()) {
            const std::string adsIdentifier = CreateAdsRelativeIdentifier(assetPath, anchorAssetPath);
            if (!adsIdentifier.empty()) {
                return adsIdentifier;
            }
            const std::filesystem::path anchor(anchorAssetPath.GetPathString());
            return NormalizeSlashes((anchor.parent_path() / assetPath).lexically_normal().string());
        }
        return NormalizeSlashes(std::filesystem::path(assetPath).lexically_normal().string());
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
        const std::string normalized = StripQuery(StripSdfFormatArgs(NormalizeSlashes(assetPath)));
        const size_t slashPos = normalized.find_last_of('/');
        const size_t dotPos = normalized.find_last_of('.');
        if (dotPos == std::string::npos || (slashPos != std::string::npos && dotPos < slashPos)) {
            return {};
        }
        return normalized.substr(dotPos + 1);
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
        if (StartsWithAdsScheme(resolvedPath.GetPathString())
            || StartsWithHttpScheme(resolvedPath.GetPathString())) {
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
