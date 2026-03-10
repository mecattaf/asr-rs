Name:           asr-rs
Version:        0.0.1
Release:        1%{?dist}
Summary:        Streaming local speech-to-text daemon for Wayland/Linux

License:        MIT
URL:            https://github.com/mecattaf/asr-rs
Source0:        %{url}/archive/v%{version}/%{name}-%{version}.tar.gz

BuildRequires:  rust-packaging
BuildRequires:  cargo
BuildRequires:  alsa-lib-devel

Recommends:     wtype
Recommends:     dotool
Recommends:     wl-clipboard

%description
asr-rs is a streaming speech-to-text daemon that captures microphone audio,
streams it over WebSocket to a local WhisperLiveKit inference server, receives
cumulative text snapshots, diffs them to extract deltas, runs post-processing
(hallucination filter, spoken punctuation, auto-capitalization), and injects
the result into the focused window via a fallback chain of output drivers.

Built for Niri on Fedora with Wayland, but works on any Wayland compositor
with PipeWire.

%prep
%autosetup -n %{name}-%{version}

%build
cargo build --release

%install
install -Dpm 0755 target/release/%{name} %{buildroot}%{_bindir}/%{name}
install -Dpm 0644 packaging/%{name}.service %{buildroot}%{_userunitdir}/%{name}.service

%files
%license LICENSE
%doc README.md
%{_bindir}/%{name}
%{_userunitdir}/%{name}.service

%post
%systemd_user_post %{name}.service

%preun
%systemd_user_preun %{name}.service

%postun
%systemd_user_postun_with_restart %{name}.service

%changelog
* Tue Mar 10 2026 mecattaf - 0.0.1-1
- Initial package
