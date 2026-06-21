cask "openless" do
  arch arm: "aarch64", intel: "x64"

  version "1.3.11"
  sha256 arm:   "2d942e795daa8389c7cc8bdcbaa293bbba84fe54d29993c6166e62f9c2563b5d",
         intel: "a8ba2ea8b42feb36c9088576bdf57020d56b08d4e3f0fab64a8207f50b5567b7"

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
