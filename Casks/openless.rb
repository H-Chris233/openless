cask "openless" do
  arch arm: "aarch64", intel: "x64"

  version "1.3.10"
  sha256 arm:   "f89a196428009b372ceb8e08373d3b49305f3ba50d80f067bf39a194fc73c9a6",
         intel: "a2a248c1d5cbe130e81d88a399311f75687ba1ad2823590c16b38f4038b0efd4"

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
