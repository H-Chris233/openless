cask "openless" do
  arch arm: "aarch64", intel: "x64"

  version "1.3.8"
  sha256 arm:   "95d2cb586cb2aa140b215827bc42dbcc4c858ce47c93c4eadb6ea8d73f9a97de",
         intel: "79c59f5075c99a0bfa95359a721ad850237dcdc779b0a931a0165aada0bde71b"

  url "https://github.com/appergb/openless/releases/download/v#{version}-tauri/OpenLess_#{version}_#{arch}.dmg"
  name "OpenLess"
  desc "Menu-bar voice input layer for macOS"
  homepage "https://github.com/appergb/openless"

  livecheck do
    url :url
    regex(/^v?(\d+(?:\.\d+)+)[._-]tauri$/i)
  end

  auto_updates true

  app "OpenLess.app"

  zap trash: [
    "~/Library/Application Support/OpenLess",
    "~/Library/Caches/com.openless.app",
    "~/Library/Logs/OpenLess",
    "~/Library/Preferences/com.openless.app.plist",
    "~/Library/WebKit/com.openless.app",
  ]
end
