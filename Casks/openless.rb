cask "openless" do
  arch arm: "aarch64", intel: "x64"

  version "1.3.5"
  sha256 arm:   "88cd4828d18812c80bb05c96ef2346cddf61360d8392b09bf4426af7fce2031a",
         intel: "6fa1960f39a103cd2d4656429406e70a7ae6f1f344948bd9916b961e9494c622"

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
