# SPDX-License-Identifier: GPL-3.0-or-later
# DexHid Fedora spec (M4). Builds from a `cargo vendor` tarball (offline); the app-menu entries, the D-Bus
# activation file and the units come from data/ (wf/dexhid/pkg/prep.sh uses them as they are).
# Before a Fedora review: rust2rpm with packaged crates where they exist, %%cargo_license_summary, check the
# current Rust vendoring guideline. Licence: GPL-3.0-or-later (LICENSE).

%global debug_package %{nil}

Name:           dexhid
Version:        0.1.0
Release:        0.4.m4%{?dist}
Summary:        Tablet screen as touchpad and keyboard for an external display
License:        GPL-3.0-or-later
URL:            https://github.com/mzakamin-hub/DexHid
Source0:        %{name}-%{version}.tar.gz
Source1:        %{name}-%{version}-vendor.tar.gz

ExclusiveArch:  %{rust_arches}
BuildRequires:  cargo-rpm-macros >= 26
BuildRequires:  systemd-rpm-macros
BuildRequires:  desktop-file-utils
Requires:       libxkbcommon
Recommends:     libinput-utils
# the mirror preview (gst-launch-1.0 with the PipeWire screencast source)
Recommends:     gstreamer1-plugins-base
Recommends:     pipewire-gstreamer

%description
DexHid turns a tablet's touchscreen into a touchpad and an on-screen PC keyboard while an external display
is connected over USB-C. It grabs the internal touchscreen, emits a virtual multitouch touchpad and keyboard
through uinput (so the desktop's libinput, xkb layout and key repeat apply) and draws its UI as a layer-shell
overlay on the internal panel. Distribution- and compositor-agnostic; a session D-Bus API (org.dexhid.DexHid1)
and CLI control it.

%prep
%autosetup -n DexHid-%{version} -a1
%cargo_prep -v vendor

%build
%cargo_build
%{cargo_vendor_manifest}
%{cargo_license_summary}
%{cargo_license} > LICENSE.dependencies

%install
install -Dpm 0755 target/rpm/dexhid %{buildroot}%{_bindir}/dexhid
install -Dpm 0644 data/dexhid.service %{buildroot}%{_userunitdir}/dexhid.service
install -Dpm 0644 data/dexhid-hid.socket %{buildroot}%{_unitdir}/dexhid-hid.socket
install -Dpm 0644 data/dexhid-hid.service %{buildroot}%{_unitdir}/dexhid-hid.service
install -Dpm 0644 data/70-dexhid.rules %{buildroot}%{_udevrulesdir}/70-dexhid.rules
install -Dpm 0644 data/org.dexhid.DexHid1.xml %{buildroot}%{_datadir}/dbus-1/interfaces/org.dexhid.DexHid1.xml
install -Dpm 0644 data/dexhid.desktop %{buildroot}%{_sysconfdir}/xdg/autostart/dexhid.desktop
install -Dpm 0644 data/config.example.toml %{buildroot}%{_datadir}/dexhid/config.example.toml
install -Dpm 0644 -t %{buildroot}%{_datadir}/dexhid/layouts data/layouts/*.toml
install -Dpm 0644 -t %{buildroot}%{_datadir}/dexhid/devices data/devices/*.toml
install -Dpm 0644 -t %{buildroot}%{_datadir}/applications data/org.dexhid.DexHid.desktop data/org.dexhid.DexHid.Settings.desktop
install -Dpm 0644 data/org.dexhid.DexHid1.service %{buildroot}%{_datadir}/dbus-1/services/org.dexhid.DexHid1.service

%check
desktop-file-validate %{buildroot}%{_datadir}/applications/org.dexhid.DexHid.desktop
desktop-file-validate %{buildroot}%{_datadir}/applications/org.dexhid.DexHid.Settings.desktop
%cargo_test

%post
%systemd_user_post dexhid.service
%systemd_post dexhid-hid.socket dexhid-hid.service
%udev_rules_update

%preun
%systemd_user_preun dexhid.service
%systemd_preun dexhid-hid.socket dexhid-hid.service

%postun
%systemd_user_postun_with_restart dexhid.service
%systemd_postun_with_restart dexhid-hid.service
%udev_rules_update

%files
%license LICENSE LICENSE.dependencies cargo-vendor.txt
%{_bindir}/dexhid
%{_userunitdir}/dexhid.service
%{_unitdir}/dexhid-hid.socket
%{_unitdir}/dexhid-hid.service
%{_udevrulesdir}/70-dexhid.rules
%{_datadir}/dbus-1/interfaces/org.dexhid.DexHid1.xml
%{_datadir}/dbus-1/services/org.dexhid.DexHid1.service
%{_datadir}/applications/org.dexhid.DexHid.desktop
%{_datadir}/applications/org.dexhid.DexHid.Settings.desktop
%config(noreplace) %{_sysconfdir}/xdg/autostart/dexhid.desktop
%{_datadir}/dexhid/

%changelog
* Sun Nov 08 2026 DexHid developers - 0.1.0-0.4.m4
- M4: new look (design tokens in mm, anti-aliased shapes, vector icons, light / dark / desktop colours), the app
  window (dexhid window / dexhid settings: Home with the 3-way mode switch and the layout picker, all settings),
  D-Bus OpenWindow / OpenSettings / SetMode, dexhid mode usb|pad|screen, settings --json for scripts
- M4: idle = no wake-ups (no UI / guardian / helper timers, idle watchdog timeout, patched blocking pool),
  partial repaints (Shift, timer, settings taps), no static screen copy, small prompt surface, half-resolution
  option; app-menu entries from data/ (actions USB keyboard + mouse, Keyboard + touchpad, Second screen),
  DexHid Settings visible again, D-Bus activation file
- fixes: the USB target follows the desktop layout by its xkb code (+ variant); On / Off accepted by dexhid set

* Sun Nov 08 2026 DexHid developers - 0.1.0-0.3.m3
- app menu: DexHid actions "USB keyboard + mouse" (target usb) and "Back to the tablet desktop" (target desktop +
  release); DexHid Settings hidden until the standalone settings window exists

* Sun Nov 08 2026 DexHid developers - 0.1.0-0.2.m3
- M2, M2.1 and M3 (USB keyboard + mouse mode, root helper socket); app-menu entries DexHid (toggle, actions
  Settings / back to the tablet desktop) and DexHid Settings

* Sat Nov 07 2026 DexHid developers - 0.1.0-0.1.m1
- M1: daemon, hotplug, ask prompt, pad mode (touchpad + keyboard regions), release paths, D-Bus API + CLI
