cask "openless" do
  arch arm: "aarch64", intel: "x64"

  version "1.3.10"
  sha256 arm:   "16494ba1e5677ec4bb2ad83181e3a8d12bc5ff7a906209429e171be6536e571f",
         intel: "dadd0b15421610d9fe98858b54ddd865640c9cbf2558d027efa6ffc2b3af0edd"

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
