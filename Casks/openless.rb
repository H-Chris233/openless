cask "openless" do
  arch arm: "aarch64", intel: "x64"

  version "1.3.4"
  sha256 arm:   "609e583bc3dc41467ac7c151d8fa6b11702027e1ae565bb682b0129f19db698c",
         intel: "bb77915b7b410465a1c935bc267b07e3f2a4e700dda905ab713457204867de09"

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
