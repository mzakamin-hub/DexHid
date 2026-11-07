# SPDX-License-Identifier: GPL-3.0-or-later  (PROVISIONAL)
# DexHid Fedora spec (M1 stub). Builds from a `cargo vendor` tarball (offline). Not built yet: the RPM build
# needs a Fedora aarch64 build root (mock / the tablet / Copr); the M1 test binary is cross-built in WSL instead.
# Before a Fedora review: rust2rpm with packaged crates where they exist, %cargo_license_summary, check the
# current Rust vendoring guideline; licence still provisional.

%global debug_package %{nil}

Name:           dexhid
Version:        0.1.0
Release:        0.1.m1%{?dist}
Summary:        Tablet screen as touchpad and keyboard for an external display
License:        GPL-3.0-or-later
# URL: set when the repository goes public (owner account not decided)
Source0:        %{name}-%{version}.tar.gz
Source1:        %{name}-%{version}-vendor.tar.gz

ExclusiveArch:  %{rust_arches}
BuildRequires:  cargo-rpm-macros >= 26
BuildRequires:  systemd-rpm-macros
Requires:       libxkbcommon
Recommends:     libinput-utils

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
install -Dpm 0644 data/70-dexhid.rules %{buildroot}%{_udevrulesdir}/70-dexhid.rules
install -Dpm 0644 data/org.dexhid.DexHid1.xml %{buildroot}%{_datadir}/dbus-1/interfaces/org.dexhid.DexHid1.xml
install -Dpm 0644 data/dexhid.desktop %{buildroot}%{_sysconfdir}/xdg/autostart/dexhid.desktop
install -Dpm 0644 data/config.example.toml %{buildroot}%{_datadir}/dexhid/config.example.toml
install -Dpm 0644 -t %{buildroot}%{_datadir}/dexhid/layouts data/layouts/*.toml
install -Dpm 0644 -t %{buildroot}%{_datadir}/dexhid/devices data/devices/*.toml

%check
%cargo_test

%post
%systemd_user_post dexhid.service
%udev_rules_update

%preun
%systemd_user_preun dexhid.service

%postun
%systemd_user_postun_with_restart dexhid.service
%udev_rules_update

%files
%license LICENSE.dependencies cargo-vendor.txt
%{_bindir}/dexhid
%{_userunitdir}/dexhid.service
%{_udevrulesdir}/70-dexhid.rules
%{_datadir}/dbus-1/interfaces/org.dexhid.DexHid1.xml
%config(noreplace) %{_sysconfdir}/xdg/autostart/dexhid.desktop
%{_datadir}/dexhid/

%changelog
* Sat Nov 07 2026 DexHid developers - 0.1.0-0.1.m1
- M1: daemon, hotplug, ask prompt, pad mode (touchpad + keyboard regions), release paths, D-Bus API + CLI
