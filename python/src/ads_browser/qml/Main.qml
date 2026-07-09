import QtQuick
import QtQuick.Controls
import QtQuick.Controls.Basic
import QtQuick.Layouts
import QtQuick.Dialogs

ApplicationWindow {
    id: root
    width: 1440
    height: 900
    minimumWidth: 1100
    minimumHeight: 700
    visible: true
    title: "ADS Asset Browser"
    color: "#131210"

    // Context property `ads` can evaluate null during early bindings; mirror
    // busy onto a local bool updated from the bridge signal.
    property bool bridgeBusy: false
    property string serverUrl: typeof adsInitialServer !== "undefined" ? adsInitialServer : ""
    property string token: typeof adsInitialToken !== "undefined" ? adsInitialToken : ""
    property bool unlocked: false
    property var profiles: []
    property string profile: ""
    property var allAssets: []
    property var filteredAssets: []
    property string categoryFilter: ""
    property string departmentFilter: ""
    property string searchText: ""
    property var selected: null
    property var detail: null
    property var manifestEntries: []
    property string manifestSummary: ""
    property string statusText: ""
    property string statusKind: "info"
    property var thumbUrls: ({})
    property bool forcePull: false
    property string detailThumbSha: ""
    property string detailThumbMime: ""

    function callAds(methodName) {
        if (typeof ads === "undefined" || ads === null) {
            statusText = "ADS bridge is not ready"
            statusKind = "err"
            return
        }
        var fn = ads[methodName]
        if (typeof fn !== "function") {
            statusText = "ADS bridge missing " + methodName
            statusKind = "err"
            return
        }
        switch (arguments.length) {
        case 1: return fn.call(ads)
        case 2: return fn.call(ads, arguments[1])
        case 3: return fn.call(ads, arguments[1], arguments[2])
        case 4: return fn.call(ads, arguments[1], arguments[2], arguments[3])
        case 5: return fn.call(ads, arguments[1], arguments[2], arguments[3], arguments[4])
        case 6: return fn.call(ads, arguments[1], arguments[2], arguments[3], arguments[4], arguments[5])
        case 7: return fn.call(ads, arguments[1], arguments[2], arguments[3], arguments[4], arguments[5], arguments[6])
        default:
            statusText = "ADS bridge call arity unsupported"
            statusKind = "err"
            return
        }
    }

    readonly property var categories: {
        var set = {}
        var out = []
        for (var i = 0; i < allAssets.length; i++) {
            var c = allAssets[i].category || ""
            if (!set[c]) { set[c] = true; out.push(c) }
        }
        out.sort()
        return out
    }
    readonly property var departments: {
        var set = {}
        var out = []
        for (var i = 0; i < allAssets.length; i++) {
            var d = allAssets[i].department || ""
            if (!set[d]) { set[d] = true; out.push(d) }
        }
        out.sort()
        return out
    }

    function applyFilters() {
        var q = searchText.trim().toLowerCase()
        var out = []
        for (var i = 0; i < allAssets.length; i++) {
            var a = allAssets[i]
            if (categoryFilter && a.category !== categoryFilter) continue
            if (departmentFilter && a.department !== departmentFilter) continue
            if (q) {
                var hay = (a.category + "/" + a.asset_code + "/" + a.department).toLowerCase()
                if (hay.indexOf(q) < 0) continue
            }
            out.push(a)
        }
        filteredAssets = out
    }

    function assetKey(a) {
        return a.category + "/" + a.asset_code + "/" + a.department
    }

    function selectAsset(a) {
        selected = a
        detail = null
        manifestEntries = []
        manifestSummary = ""
        detailThumbSha = ""
        detailThumbMime = ""
        callAds("loadDetail", profile, a.category, a.asset_code, a.department)
    }

    function callContextMenu(methodName) {
        if (typeof contextMenu === "undefined" || contextMenu === null) {
            statusText = "Context menu bridge is not ready"
            statusKind = "err"
            return
        }
        var fn = contextMenu[methodName]
        if (typeof fn !== "function") {
            statusText = "Context menu missing " + methodName
            statusKind = "err"
            return
        }
        switch (arguments.length) {
        case 1: return fn.call(contextMenu)
        case 2: return fn.call(contextMenu, arguments[1])
        case 3: return fn.call(contextMenu, arguments[1], arguments[2])
        case 4: return fn.call(contextMenu, arguments[1], arguments[2], arguments[3])
        case 5: return fn.call(contextMenu, arguments[1], arguments[2], arguments[3], arguments[4])
        case 6: return fn.call(contextMenu, arguments[1], arguments[2], arguments[3], arguments[4], arguments[5])
        case 7: return fn.call(contextMenu, arguments[1], arguments[2], arguments[3], arguments[4], arguments[5], arguments[6])
        case 8: return fn.call(contextMenu, arguments[1], arguments[2], arguments[3], arguments[4], arguments[5], arguments[6], arguments[7])
        case 9: return fn.call(contextMenu, arguments[1], arguments[2], arguments[3], arguments[4], arguments[5], arguments[6], arguments[7], arguments[8])
        default:
            statusText = "Context menu call arity unsupported"
            statusKind = "err"
            return
        }
    }

    function openAssetContextMenu(a) {
        selectAsset(a)
        var version = ""
        if (selected && detail && assetKey(selected) === assetKey(a) && versionCombo.count > 0)
            version = versionCombo.currentText
        else if (a.current != null)
            version = String(a.current)

        // Push inspector enrichments when they match the clicked asset.
        if (detail && selected && assetKey(selected) === assetKey(a)) {
            callContextMenu("setManifestJson", JSON.stringify(manifestEntries || []))
            var seqs = []
            var wips = detail.wips || []
            for (var i = 0; i < wips.length; i++) seqs.push(wips[i].seq)
            callContextMenu("setWipSeqsJson", JSON.stringify(seqs))
        }

        callContextMenu(
            "prepareMenuFull",
            profile,
            a.category || "",
            a.asset_code || "",
            a.department || "",
            version,
            a.thumbnail_sha256 || "",
            a.current != null ? String(a.current) : "",
            a.latest != null ? String(a.latest) : ""
        )
        assetContextMenu.popup()
    }

    function refreshAssets() {
        if (!profile) return
        callAds("refreshAssets", profile, searchText)
    }

    function thumbKey(sha) {
        return profile + ":" + sha
    }

    function requestThumb(a) {
        if (!a || !a.thumbnail_sha256) return
        var key = thumbKey(a.thumbnail_sha256)
        if (thumbUrls[key]) return
        callAds("fetchThumbnail", profile, a.thumbnail_sha256, a.thumbnail_mime_type || "")
    }

    function currentVersion() {
        if (!detail) return ""
        if (versionCombo.count > 0) return versionCombo.currentText
        return ""
    }

    function updateUriField() {
        if (!selected) { uriField.text = ""; return }
        var uri = callAds("buildUri", selected.category, selected.asset_code, selected.department, currentVersion())
        uriField.text = uri || ""
    }

    function thumbnailForVersion(version) {
        if (!detail || !selected) return { sha: "", mime: "" }
        var thumbs = detail.thumbnails || []
        var ver = String(version || "")
        for (var t = 0; t < thumbs.length; t++) {
            var rec = thumbs[t]
            if (String(rec.version) !== ver) continue
            var dk = rec.department_key || {}
            var ak = dk.asset_key || {}
            if (ak.category && ak.category !== selected.category) continue
            if (ak.asset_code && ak.asset_code !== selected.asset_code) continue
            if (dk.department && dk.department !== selected.department) continue
            return { sha: rec.sha256 || "", mime: rec.mime_type || "" }
        }
        // Fall back to the asset-card thumbnail when no version-specific record exists.
        return {
            sha: selected.thumbnail_sha256 || "",
            mime: selected.thumbnail_mime_type || ""
        }
    }

    function refreshDetailThumbnail() {
        if (!selected) {
            detailThumbSha = ""
            detailThumbMime = ""
            return
        }
        var hit = thumbnailForVersion(currentVersion())
        detailThumbSha = hit.sha
        detailThumbMime = hit.mime
        if (detailThumbSha)
            callAds("fetchThumbnail", profile, detailThumbSha, detailThumbMime)
    }

    Connections {
        target: typeof ads !== "undefined" ? ads : null
        function onBusyChanged() {
            root.bridgeBusy = ads ? ads.busy : false
        }
        function onStatusChanged(text, kind) {
            statusText = text
            statusKind = kind
        }
        function onProfilesLoaded(raw) {
            var data = JSON.parse(raw)
            profiles = data.profiles || []
            if (profiles.length && !profile) profile = profiles[0].name
            unlocked = true
            // Sync ComboBox after model rebuild
            Qt.callLater(function () {
                var idx = profileCombo.find(profile)
                if (idx >= 0) profileCombo.currentIndex = idx
            })
            refreshAssets()
        }
        function onAssetsLoaded(raw) {
            var data = JSON.parse(raw)
            allAssets = data.assets || []
            applyFilters()
            for (var i = 0; i < allAssets.length; i++) requestThumb(allAssets[i])
        }
        function onDetailLoaded(raw) {
            detail = JSON.parse(raw)
            var vers = detail.versions || []
            versionModel.clear()
            var current = detail.current_status && detail.current_status.current != null
                ? String(detail.current_status.current) : ""
            var idx = 0
            for (var i = 0; i < vers.length; i++) {
                var v = String(vers[i].version)
                versionModel.append({ label: v })
                if (current && v === current) idx = i
            }
            versionCombo.currentIndex = vers.length ? idx : -1
            updateUriField()
            if (vers.length) {
                callAds("loadManifest", profile, selected.category, selected.asset_code, selected.department, versionCombo.currentText)
            }
            refreshDetailThumbnail()
        }
        function onManifestLoaded(raw) {
            var info = JSON.parse(raw)
            var entries = (info.manifest && info.manifest.entries) ? info.manifest.entries : []
            manifestEntries = entries
            var bytes = 0
            for (var i = 0; i < entries.length; i++) bytes += entries[i].size || 0
            manifestSummary = entries.length + " files · " + humanBytes(bytes)
        }
        function onThumbnailReady(cacheKey, fileUrl) {
            var next = Object.assign({}, thumbUrls)
            next[cacheKey] = fileUrl
            thumbUrls = next
        }
        function onActionDone(action, raw) {
            if (!selected) return
            if (action === "promote" || action === "setCurrent" || action === "resetCurrent" || action === "uploadThumbnail") {
                callAds("loadDetail", profile, selected.category, selected.asset_code, selected.department)
                refreshAssets()
            }
        }
    }

    function humanBytes(n) {
        if (n < 1024) return n + " B"
        if (n < 1048576) return (n / 1024).toFixed(1) + " KB"
        if (n < 1073741824) return (n / 1048576).toFixed(1) + " MB"
        return (n / 1073741824).toFixed(2) + " GB"
    }

    Component.onCompleted: {
        if (typeof ads !== "undefined" && ads)
            root.bridgeBusy = ads.busy
        if (typeof adsAutoConnect !== "undefined" && adsAutoConnect && serverUrl && token) {
            callAds("connectToServer", serverUrl, token)
        }
    }

    // —— Unlock ——
    Rectangle {
        anchors.fill: parent
        color: "#131210"
        visible: !unlocked
        z: 10

        Rectangle {
            anchors.centerIn: parent
            width: 420
            height: 340
            color: "#1a1816"
            border.color: "#3d372f"
            border.width: 1
            radius: 4

            ColumnLayout {
                anchors.fill: parent
                anchors.margins: 28
                spacing: 14

                Text {
                    text: "ADS"
                    color: "#f2a93c"
                    font.family: "Bahnschrift"
                    font.pixelSize: 28
                    font.bold: true
                }
                Text {
                    text: "Asset Browser"
                    color: "#e9e4da"
                    font.family: "Bahnschrift"
                    font.pixelSize: 20
                }
                Text {
                    text: "Production asset store — authorization required"
                    color: "#9b948a"
                    font.pixelSize: 12
                    Layout.fillWidth: true
                    wrapMode: Text.Wrap
                }
                TextField {
                    id: serverField
                    Layout.fillWidth: true
                    text: root.serverUrl
                    placeholderText: "http://host:8787"
                    color: "#e9e4da"
                    placeholderTextColor: "#6e6759"
                    background: Rectangle { color: "#201d1a"; border.color: "#3d372f"; radius: 3 }
                }
                TextField {
                    id: tokenField
                    Layout.fillWidth: true
                    text: root.token
                    echoMode: TextInput.Password
                    placeholderText: "Bearer token"
                    color: "#e9e4da"
                    placeholderTextColor: "#6e6759"
                    background: Rectangle { color: "#201d1a"; border.color: "#3d372f"; radius: 3 }
                    Keys.onReturnPressed: unlockBtn.clicked()
                }
                Button {
                    id: unlockBtn
                    Layout.fillWidth: true
                    text: "Unlock"
                    onClicked: {
                        root.serverUrl = serverField.text.trim()
                        root.token = tokenField.text.trim()
                        callAds("connectToServer", root.serverUrl, root.token)
                    }
                    contentItem: Text {
                        text: parent.text
                        color: "#131210"
                        horizontalAlignment: Text.AlignHCenter
                        font.bold: true
                    }
                    background: Rectangle { color: "#f2a93c"; radius: 3 }
                }
            }
        }
    }

    // —— Main shell ——
    ColumnLayout {
        anchors.fill: parent
        spacing: 0
        visible: unlocked

        // Header
        Rectangle {
            Layout.fillWidth: true
            Layout.preferredHeight: 56
            color: "#1a1816"
            border.color: "#2c2823"
            border.width: 1

            RowLayout {
                anchors.fill: parent
                anchors.leftMargin: 16
                anchors.rightMargin: 16
                spacing: 16

                Row {
                    spacing: 10
                    Text { text: "ADS"; color: "#f2a93c"; font.family: "Bahnschrift"; font.pixelSize: 18; font.bold: true }
                    Text { text: "Asset Browser"; color: "#9b948a"; font.family: "Bahnschrift"; font.pixelSize: 14; anchors.verticalCenter: parent.verticalCenter }
                }

                ComboBox {
                    id: profileCombo
                    Layout.preferredWidth: 140
                    model: {
                        var names = []
                        for (var i = 0; i < profiles.length; i++) names.push(profiles[i].name)
                        return names
                    }
                    onActivated: {
                        profile = currentText
                        categoryFilter = ""
                        departmentFilter = ""
                        refreshAssets()
                    }
                    background: Rectangle { color: "#201d1a"; border.color: "#3d372f"; radius: 3 }
                    contentItem: Text { text: profileCombo.displayText; color: "#e9e4da"; leftPadding: 8; verticalAlignment: Text.AlignVCenter }
                }

                TextField {
                    id: searchField
                    Layout.fillWidth: true
                    placeholderText: "Search assets, categories, departments…"
                    color: "#e9e4da"
                    placeholderTextColor: "#6e6759"
                    background: Rectangle { color: "#201d1a"; border.color: "#3d372f"; radius: 3 }
                    onTextChanged: {
                        searchDebounce.restart()
                    }
                }
                Timer {
                    id: searchDebounce
                    interval: 250
                    onTriggered: {
                        searchText = searchField.text
                        applyFilters()
                        refreshAssets()
                    }
                }

                Text {
                    text: "SCHEMA V8"
                    color: "#6e6759"
                    font.pixelSize: 11
                    font.family: "Consolas"
                }
                Rectangle {
                    width: 10; height: 10; radius: 5
                    color: bridgeBusy ? "#f2a93c" : (statusKind === "err" ? "#e0604a" : "#8cd97c")
                }
                Button {
                    text: "Rescan"
                    onClicked: refreshAssets()
                    contentItem: Text { text: parent.text; color: "#e9e4da"; horizontalAlignment: Text.AlignHCenter }
                    background: Rectangle { color: "transparent"; border.color: "#3d372f"; radius: 3 }
                }
                Button {
                    text: "Lock"
                    onClicked: { unlocked = false; selected = null; detail = null }
                    contentItem: Text { text: parent.text; color: "#e9e4da"; horizontalAlignment: Text.AlignHCenter }
                    background: Rectangle { color: "transparent"; border.color: "#3d372f"; radius: 3 }
                }
            }
        }

        RowLayout {
            Layout.fillWidth: true
            Layout.fillHeight: true
            spacing: 0

            // Rail
            Rectangle {
                Layout.preferredWidth: 180
                Layout.fillHeight: true
                color: "#1a1816"
                border.color: "#2c2823"

                ColumnLayout {
                    anchors.fill: parent
                    anchors.margins: 12
                    spacing: 16

                    Text { text: "CATEGORY"; color: "#6e6759"; font.pixelSize: 10; font.letterSpacing: 1 }
                    ListView {
                        Layout.fillWidth: true
                        Layout.preferredHeight: Math.min(220, categories.length * 28 + 28)
                        model: ["(all)"].concat(categories)
                        clip: true
                        delegate: Text {
                            required property string modelData
                            width: ListView.view.width
                            height: 26
                            text: modelData
                            color: {
                                var active = (modelData === "(all)" && categoryFilter === "") || modelData === categoryFilter
                                return active ? "#f2a93c" : "#e9e4da"
                            }
                            font.pixelSize: 13
                            MouseArea {
                                anchors.fill: parent
                                cursorShape: Qt.PointingHandCursor
                                onClicked: {
                                    categoryFilter = modelData === "(all)" ? "" : modelData
                                    applyFilters()
                                }
                            }
                        }
                    }

                    Text { text: "DEPARTMENT"; color: "#6e6759"; font.pixelSize: 10; font.letterSpacing: 1 }
                    ListView {
                        Layout.fillWidth: true
                        Layout.fillHeight: true
                        model: ["(all)"].concat(departments)
                        clip: true
                        delegate: Text {
                            required property string modelData
                            width: ListView.view.width
                            height: 26
                            text: modelData
                            color: {
                                var active = (modelData === "(all)" && departmentFilter === "") || modelData === departmentFilter
                                return active ? "#f2a93c" : "#e9e4da"
                            }
                            font.pixelSize: 13
                            MouseArea {
                                anchors.fill: parent
                                cursorShape: Qt.PointingHandCursor
                                onClicked: {
                                    departmentFilter = modelData === "(all)" ? "" : modelData
                                    applyFilters()
                                }
                            }
                        }
                    }

                    Text {
                        Layout.fillWidth: true
                        text: statusText
                        color: statusKind === "err" ? "#e0604a" : (statusKind === "ok" ? "#8cd97c" : "#9b948a")
                        font.pixelSize: 11
                        wrapMode: Text.Wrap
                    }
                }
            }

            // Grid
            Rectangle {
                Layout.fillWidth: true
                Layout.fillHeight: true
                color: "#131210"

                ColumnLayout {
                    anchors.fill: parent
                    anchors.margins: 12
                    spacing: 8

                    Text {
                        text: filteredAssets.length + " ASSETS"
                        color: "#6e6759"
                        font.pixelSize: 11
                        font.family: "Consolas"
                    }

                    GridView {
                        id: grid
                        Layout.fillWidth: true
                        Layout.fillHeight: true
                        cellWidth: 168
                        cellHeight: 196
                        clip: true
                        model: filteredAssets
                        delegate: Rectangle {
                            required property var modelData
                            required property int index
                            width: 156
                            height: 184
                            color: selected && assetKey(selected) === assetKey(modelData) ? "#272320" : "#1a1816"
                            border.color: selected && assetKey(selected) === assetKey(modelData) ? "#f2a93c" : "#2c2823"
                            border.width: 1
                            radius: 3

                            Column {
                                anchors.fill: parent
                                anchors.margins: 8
                                spacing: 6

                                Rectangle {
                                    width: parent.width
                                    height: 110
                                    color: "#201d1a"
                                    radius: 2
                                    Image {
                                        anchors.fill: parent
                                        anchors.margins: 2
                                        fillMode: Image.PreserveAspectCrop
                                        asynchronous: true
                                        source: {
                                            var sha = modelData.thumbnail_sha256 || ""
                                            return sha ? (thumbUrls[thumbKey(sha)] || "") : ""
                                        }
                                    }
                                    Text {
                                        anchors.centerIn: parent
                                        visible: !(modelData.thumbnail_sha256 && thumbUrls[thumbKey(modelData.thumbnail_sha256)])
                                        text: (modelData.asset_code || "?").charAt(0).toUpperCase()
                                        color: "#3d372f"
                                        font.pixelSize: 36
                                        font.family: "Bahnschrift"
                                    }
                                }
                                Text {
                                    width: parent.width
                                    text: modelData.asset_code || ""
                                    color: "#e9e4da"
                                    font.pixelSize: 13
                                    font.bold: true
                                    elide: Text.ElideRight
                                }
                                Text {
                                    width: parent.width
                                    text: (modelData.category || "") + " · " + (modelData.department || "")
                                    color: "#9b948a"
                                    font.pixelSize: 11
                                    elide: Text.ElideRight
                                }
                                Text {
                                    text: "v" + (modelData.current != null ? modelData.current : "—")
                                    color: "#f2a93c"
                                    font.pixelSize: 11
                                    font.family: "Consolas"
                                }
                            }
                            MouseArea {
                                anchors.fill: parent
                                acceptedButtons: Qt.LeftButton | Qt.RightButton
                                cursorShape: Qt.PointingHandCursor
                                onClicked: (mouse) => {
                                    if (mouse.button === Qt.RightButton) {
                                        openAssetContextMenu(modelData)
                                    } else {
                                        selectAsset(modelData)
                                    }
                                }
                            }
                        }
                    }

                    Text {
                        visible: filteredAssets.length === 0
                        text: "No assets match the current filters."
                        color: "#6e6759"
                        Layout.alignment: Qt.AlignHCenter
                    }
                }
            }

            // Inspector
            Rectangle {
                Layout.preferredWidth: 340
                Layout.fillHeight: true
                color: "#1a1816"
                border.color: "#2c2823"

                ColumnLayout {
                    anchors.fill: parent
                    anchors.margins: 14
                    spacing: 12
                    visible: selected !== null && detail !== null

                    RowLayout {
                        Layout.fillWidth: true
                        Text {
                            text: selected ? selected.asset_code : ""
                            color: "#e9e4da"
                            font.pixelSize: 20
                            font.family: "Bahnschrift"
                            font.bold: true
                            Layout.fillWidth: true
                            elide: Text.ElideRight
                        }
                        Rectangle {
                            radius: 3
                            color: "#272320"
                            border.color: "#3d372f"
                            implicitWidth: deptLabel.implicitWidth + 12
                            implicitHeight: 22
                            Text {
                                id: deptLabel
                                anchors.centerIn: parent
                                text: selected ? selected.department : ""
                                color: "#f2a93c"
                                font.pixelSize: 11
                            }
                        }
                    }
                    Text {
                        text: selected ? (selected.category + "/" + selected.asset_code + "/" + selected.department) : ""
                        color: "#9b948a"
                        font.family: "Consolas"
                        font.pixelSize: 11
                    }

                    Rectangle {
                        Layout.fillWidth: true
                        Layout.preferredHeight: 160
                        color: "#201d1a"
                        radius: 3
                        Image {
                            id: detailThumb
                            anchors.fill: parent
                            anchors.margins: 4
                            fillMode: Image.PreserveAspectFit
                            asynchronous: true
                            source: detailThumbSha ? (thumbUrls[thumbKey(detailThumbSha)] || "") : ""
                        }
                        Text {
                            anchors.centerIn: parent
                            visible: !detailThumbSha || !thumbUrls[thumbKey(detailThumbSha)]
                            text: detailThumbSha ? "…" : "NO THUMBNAIL"
                            color: "#6e6759"
                            font.pixelSize: 12
                        }
                    }

                    Text { text: "ADS URI"; color: "#6e6759"; font.pixelSize: 10; font.letterSpacing: 1 }
                    RowLayout {
                        Layout.fillWidth: true
                        TextField {
                            id: uriField
                            Layout.fillWidth: true
                            readOnly: true
                            color: "#e9e4da"
                            font.family: "Consolas"
                            font.pixelSize: 11
                            background: Rectangle { color: "#201d1a"; border.color: "#3d372f"; radius: 3 }
                        }
                        Button {
                            text: "Copy"
                            onClicked: callAds("copyText", uriField.text)
                            contentItem: Text { text: parent.text; color: "#131210"; horizontalAlignment: Text.AlignHCenter; font.bold: true }
                            background: Rectangle { color: "#f2a93c"; radius: 3 }
                        }
                    }

                    Text { text: "VERSION"; color: "#6e6759"; font.pixelSize: 10; font.letterSpacing: 1 }
                    RowLayout {
                        Layout.fillWidth: true
                        ComboBox {
                            id: versionCombo
                            Layout.fillWidth: true
                            model: ListModel { id: versionModel }
                            textRole: "label"
                            onActivated: {
                                updateUriField()
                                refreshDetailThumbnail()
                                if (selected)
                                    callAds("loadManifest", profile, selected.category, selected.asset_code, selected.department, currentText)
                            }
                            background: Rectangle { color: "#201d1a"; border.color: "#3d372f"; radius: 3 }
                            contentItem: Text { text: versionCombo.displayText; color: "#e9e4da"; leftPadding: 8; verticalAlignment: Text.AlignVCenter }
                        }
                        // Custom toggle — CheckBox contentItem overrides break under native styles.
                        Item {
                            id: forceToggle
                            Layout.preferredWidth: forceRow.implicitWidth
                            Layout.preferredHeight: 22
                            Layout.alignment: Qt.AlignVCenter
                            Row {
                                id: forceRow
                                spacing: 6
                                anchors.verticalCenter: parent.verticalCenter
                                Rectangle {
                                    width: 16
                                    height: 16
                                    radius: 2
                                    color: forcePull ? "#f2a93c" : "#201d1a"
                                    border.color: forcePull ? "#f2a93c" : "#3d372f"
                                    Text {
                                        anchors.centerIn: parent
                                        visible: forcePull
                                        text: "✓"
                                        color: "#131210"
                                        font.pixelSize: 11
                                        font.bold: true
                                    }
                                }
                                Text {
                                    text: "Force"
                                    color: "#9b948a"
                                    font.pixelSize: 12
                                    anchors.verticalCenter: parent.verticalCenter
                                }
                            }
                            MouseArea {
                                anchors.fill: parent
                                cursorShape: Qt.PointingHandCursor
                                onClicked: forcePull = !forcePull
                            }
                        }
                    }
                    RowLayout {
                        Layout.fillWidth: true
                        Button {
                            text: "Pin Current"
                            Layout.fillWidth: true
                            onClicked: if (selected) callAds("setCurrent", profile, selected.category, selected.asset_code, selected.department, currentVersion())
                            contentItem: Text { text: parent.text; color: "#131210"; horizontalAlignment: Text.AlignHCenter; font.bold: true }
                            background: Rectangle { color: "#f2a93c"; radius: 3 }
                        }
                        Button {
                            text: "Reset"
                            onClicked: if (selected) callAds("resetCurrent", profile, selected.category, selected.asset_code, selected.department)
                            contentItem: Text { text: parent.text; color: "#e9e4da"; horizontalAlignment: Text.AlignHCenter }
                            background: Rectangle { color: "transparent"; border.color: "#3d372f"; radius: 3 }
                        }
                    }
                    Button {
                        Layout.fillWidth: true
                        text: "Pull to Workspace"
                        onClicked: if (selected) callAds("pull", profile, selected.category, selected.asset_code, selected.department, currentVersion(), forcePull)
                        contentItem: Text { text: parent.text; color: "#e9e4da"; horizontalAlignment: Text.AlignHCenter; font.bold: true }
                        background: Rectangle { color: "#272320"; border.color: "#3d372f"; radius: 3 }
                    }

                    Text { text: "WIP STREAM"; color: "#6e6759"; font.pixelSize: 10; font.letterSpacing: 1 }
                    ListView {
                        Layout.fillWidth: true
                        Layout.preferredHeight: 90
                        clip: true
                        model: detail && detail.wips ? detail.wips : []
                        delegate: RowLayout {
                            required property var modelData
                            width: ListView.view.width
                            height: 28
                            Text {
                                text: "#" + modelData.seq + " · " + (modelData.file_count || 0) + " files"
                                color: "#e9e4da"
                                font.family: "Consolas"
                                font.pixelSize: 12
                                Layout.fillWidth: true
                            }
                            Button {
                                text: "PROMOTE"
                                onClicked: callAds("promote", profile, selected.category, selected.asset_code, selected.department, modelData.seq)
                                contentItem: Text { text: parent.text; color: "#f2a93c"; font.pixelSize: 10; horizontalAlignment: Text.AlignHCenter }
                                background: Rectangle { color: "transparent"; border.color: "#f2a93c"; radius: 3 }
                            }
                        }
                        Text {
                            anchors.centerIn: parent
                            visible: !(detail && detail.wips && detail.wips.length)
                            text: "No WIP versions"
                            color: "#6e6759"
                            font.pixelSize: 12
                        }
                    }

                    RowLayout {
                        Text { text: "MANIFEST"; color: "#6e6759"; font.pixelSize: 10; font.letterSpacing: 1; Layout.fillWidth: true }
                        Text { text: manifestSummary; color: "#9b948a"; font.pixelSize: 10; font.family: "Consolas" }
                    }
                    ListView {
                        Layout.fillWidth: true
                        Layout.fillHeight: true
                        clip: true
                        model: manifestEntries
                        delegate: Text {
                            required property var modelData
                            width: ListView.view.width
                            height: 20
                            text: modelData.relative_path || ""
                            color: "#c8c2b6"
                            font.family: "Consolas"
                            font.pixelSize: 11
                            elide: Text.ElideMiddle
                        }
                    }

                    RowLayout {
                        Layout.fillWidth: true
                        Button {
                            text: "Upload Thumbnail"
                            onClicked: thumbDialog.open()
                            contentItem: Text { text: parent.text; color: "#e9e4da"; horizontalAlignment: Text.AlignHCenter }
                            background: Rectangle { color: "transparent"; border.color: "#3d372f"; radius: 3 }
                        }
                    }
                }

                Text {
                    anchors.centerIn: parent
                    visible: selected === null
                    text: "Select an asset to inspect"
                    color: "#6e6759"
                }
                BusyIndicator {
                    anchors.centerIn: parent
                    running: selected !== null && detail === null && bridgeBusy
                    visible: running
                }
            }
        }
    }

    FileDialog {
        id: thumbDialog
        title: "Upload thumbnail"
        nameFilters: ["Images (*.png *.jpg *.jpeg *.webp)"]
        onAccepted: {
            if (!selected) return
            callAds("uploadThumbnail", profile, selected.category, selected.asset_code, selected.department, currentVersion(), selectedFile.toString())
        }
    }

    Menu {
        id: assetContextMenu
        Instantiator {
            model: typeof contextMenuModel !== "undefined" ? contextMenuModel : null
            delegate: MenuItem {
                required property int index
                required property string label
                text: label
                onTriggered: callContextMenu("invoke", index)
            }
            onObjectAdded: (index, object) => assetContextMenu.insertItem(index, object)
            onObjectRemoved: (index, object) => assetContextMenu.removeItem(object)
        }
    }
}
