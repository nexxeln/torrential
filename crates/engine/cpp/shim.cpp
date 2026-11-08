#include "torrential-engine/cpp/shim.h"

#include <algorithm>
#include <cstring>
#include <filesystem>
#include <iterator>
#include <stdexcept>
#include <vector>

#include <libtorrent/alert_types.hpp>
#include <libtorrent/bencode.hpp>
#include <libtorrent/create_torrent.hpp>
#include <libtorrent/entry.hpp>
#include <libtorrent/extensions.hpp>
#include <libtorrent/ip_filter.hpp>
#include <libtorrent/load_torrent.hpp>
#include <libtorrent/magnet_uri.hpp>
#include <libtorrent/peer_connection_handle.hpp>
#include <libtorrent/peer_request.hpp>
#include <libtorrent/read_resume_data.hpp>
#include <libtorrent/session_params.hpp>
#include <libtorrent/session_stats.hpp>
#include <libtorrent/torrent_info.hpp>
#include <libtorrent/version.hpp>
#include <libtorrent/write_resume_data.hpp>

#include "torrential-engine/src/bridge.rs.h"

namespace torrential {
namespace {

struct Error : std::runtime_error {
  using std::runtime_error::runtime_error;
};

std::string failure_text(FailureKind kind, std::string const& message) {
  return std::to_string(static_cast<int>(kind)) + ":" + message;
}

[[noreturn]] void fail(FailureKind kind, std::string const& message) {
  throw Error(failure_text(kind, message));
}

rust::String str(std::string const& s) { return rust::String::lossy(s); }

lt::span<char const> as_span(rust::Slice<const std::uint8_t> bytes) {
  return {reinterpret_cast<char const*>(bytes.data()), static_cast<std::ptrdiff_t>(bytes.size())};
}

lt::span<char const> as_span(rust::Vec<std::uint8_t> const& bytes) {
  return {reinterpret_cast<char const*>(bytes.data()), static_cast<std::ptrdiff_t>(bytes.size())};
}

rust::Vec<std::uint8_t> to_vec(std::vector<char> const& buf) {
  rust::Vec<std::uint8_t> out;
  out.reserve(buf.size());
  for (char c : buf) out.push_back(static_cast<std::uint8_t>(c));
  return out;
}

Hashes to_hashes(lt::info_hash_t const& ih) {
  Hashes h{};
  h.has_v1 = ih.has_v1();
  h.has_v2 = ih.has_v2();
  if (h.has_v1) std::memcpy(h.v1.data(), ih.v1.data(), h.v1.size());
  if (h.has_v2) std::memcpy(h.v2.data(), ih.v2.data(), h.v2.size());
  return h;
}

ErrorRecord to_error(lt::error_code const& ec) {
  ErrorRecord e{};
  if (!ec) return e;
  e.category = str(ec.category().name());
  e.code = ec.value();
  e.message = str(ec.message());
  return e;
}

State to_state(lt::torrent_status::state_t s) {
  switch (s) {
    case lt::torrent_status::checking_files: return State::CheckingFiles;
    case lt::torrent_status::downloading_metadata: return State::DownloadingMetadata;
    case lt::torrent_status::downloading: return State::Downloading;
    case lt::torrent_status::finished: return State::Finished;
    case lt::torrent_status::seeding: return State::Seeding;
    case lt::torrent_status::checking_resume_data: return State::CheckingResumeData;
  }
  fail(FailureKind::Native, "unknown torrent state " + std::to_string(static_cast<int>(s)));
}

Transport to_transport(lt::socket_type_t t) {
  switch (t) {
    case lt::socket_type_t::tcp: return Transport::Tcp;
    case lt::socket_type_t::tcp_ssl: return Transport::TcpSsl;
    case lt::socket_type_t::utp: return Transport::Utp;
    case lt::socket_type_t::utp_ssl: return Transport::UtpSsl;
    case lt::socket_type_t::socks5: return Transport::Socks5;
    case lt::socket_type_t::socks5_ssl: return Transport::Socks5Ssl;
    case lt::socket_type_t::http: return Transport::Http;
    case lt::socket_type_t::http_ssl: return Transport::HttpSsl;
    case lt::socket_type_t::i2p: return Transport::I2p;
    case lt::socket_type_t::rtc: return Transport::Rtc;
  }
  fail(FailureKind::Native, "unknown socket type " + std::to_string(static_cast<int>(t)));
}

rust::Vec<FileRecord> file_records(lt::file_storage const& fs, lt::renamed_files const& names) {
  rust::Vec<FileRecord> out;
  for (lt::file_index_t const f : fs.file_range()) {
    if (fs.pad_file_at(f)) continue;
    out.push_back(FileRecord{static_cast<std::uint32_t>(static_cast<int>(f)),
                             str(names.file_path(fs, f)),
                             static_cast<std::uint64_t>(fs.file_size(f))});
  }
  return out;
}

ParsedTorrent parsed(lt::add_torrent_params const& atp) {
  ParsedTorrent p{};
  if (atp.ti && atp.ti->is_valid()) {
    lt::file_storage const& fs = atp.ti->layout();
    lt::renamed_files names;
    names.import_filenames(fs, atp.renamed_files);
    p.hashes = to_hashes(atp.ti->info_hashes());
    p.name = str(atp.ti->name());
    p.has_metadata = true;
    p.piece_length = static_cast<std::uint32_t>(atp.ti->piece_length());
    p.files = file_records(fs, names);
  } else {
    p.hashes = to_hashes(atp.info_hashes);
    p.name = str(atp.name);
  }
  if (!p.hashes.has_v1 && !p.hashes.has_v2) fail(FailureKind::InvalidInput, "no info hash");
  for (auto const& seed : atp.url_seeds) p.web_seeds.push_back(str(seed));
  return p;
}

lt::add_torrent_params load_metainfo(lt::span<char const> bytes) {
  try {
    return lt::load_torrent_buffer(bytes);
  } catch (lt::system_error const& e) {
    fail(FailureKind::InvalidInput, std::string("metainfo: ") + e.code().message());
  }
}

lt::add_torrent_params load_resume_data(lt::span<char const> bytes) {
  lt::error_code ec;
  lt::add_torrent_params atp = lt::read_resume_data(bytes, ec);
  if (ec) fail(FailureKind::InvalidInput, "resume data: " + ec.message());
  return atp;
}

lt::add_torrent_params load_magnet(rust::Str uri) {
  lt::error_code ec;
  lt::add_torrent_params atp = lt::parse_magnet_uri(lt::string_view(uri.data(), uri.size()), ec);
  if (ec) fail(FailureKind::InvalidInput, "magnet: " + ec.message());
  return atp;
}

lt::address parse_address(rust::Str address) {
  lt::error_code ec;
  lt::address const a = lt::make_address(std::string(address), ec);
  if (ec) fail(FailureKind::InvalidInput, "address " + std::string(address) + ": " + ec.message());
  return a;
}

// libtorrent asks peer plugins before it serves any block request, so refusing
// every request is what holds upload at zero. The choker settings in
// make_params keep peers choked, so they have no reason to send one.
struct RefuseRequests final : lt::peer_plugin {
  explicit RefuseRequests(lt::peer_connection_handle pc) : pc_(std::move(pc)) {}
  bool on_request(lt::peer_request const&) override {
    pc_.choke_this_peer();
    return true;
  }

 private:
  lt::peer_connection_handle pc_;
};

struct RefuseRequestsTorrent final : lt::torrent_plugin {
  std::shared_ptr<lt::peer_plugin> new_connection(lt::peer_connection_handle const& pc) override {
    return std::make_shared<RefuseRequests>(pc);
  }
};

struct NoUpload final : lt::plugin {
  std::shared_ptr<lt::torrent_plugin> new_torrent(lt::torrent_handle const&, lt::client_data_t) override {
    return std::make_shared<RefuseRequestsTorrent>();
  }
};

lt::session_params make_params(SessionSettings const& s) {
  lt::session_params params;
  if (!s.state.empty()) {
    try {
      params = lt::read_session_params(as_span(s.state), lt::session_handle::save_dht_state);
    } catch (std::exception const& e) {
      fail(FailureKind::InvalidInput, std::string("session state: ") + e.what());
    }
  }

  lt::settings_pack& p = params.settings;
  p.set_str(lt::settings_pack::listen_interfaces, std::string(s.listen_interfaces));
  p.set_bool(lt::settings_pack::enable_dht, s.dht);
  p.set_str(lt::settings_pack::dht_bootstrap_nodes, std::string(s.dht_bootstrap_nodes));
  p.set_bool(lt::settings_pack::enable_lsd, s.lsd);
  p.set_bool(lt::settings_pack::enable_upnp, s.upnp);
  p.set_bool(lt::settings_pack::enable_natpmp, s.natpmp);
  p.set_int(lt::settings_pack::download_rate_limit, s.download_rate_limit);
  p.set_int(lt::settings_pack::upload_rate_limit, s.upload_rate_limit);
  p.set_int(lt::settings_pack::connections_limit, s.connections_limit);
  p.set_bool(lt::settings_pack::allow_multiple_connections_per_ip, s.multiple_connections_per_ip);
  p.set_str(lt::settings_pack::peer_fingerprint, std::string(s.peer_fingerprint));
  p.set_str(lt::settings_pack::user_agent, std::string(s.user_agent));
  // A finished torrent otherwise disconnects every seed, and a metadata-only
  // torrent is finished as soon as its metadata arrives. Selecting files then
  // waits until libtorrent retries those seeds, min_reconnect_time (60 s) or
  // more later.
  p.set_bool(lt::settings_pack::close_redundant_connections, false);
  // libtorrent counts interesting web seeds in order, including connected ones,
  // so redirected seeds can fill the slots. 0 raises the limit from 3 to 100.
  p.set_int(lt::settings_pack::max_web_seed_connections, 0);
  p.set_int(lt::settings_pack::alert_queue_size, 10000);
  p.set_int(lt::settings_pack::alert_mask, lt::alert_category::status | lt::alert_category::error |
                                               lt::alert_category::storage | lt::alert_category::dht);
  if (s.upload_off) {
    p.set_int(lt::settings_pack::choking_algorithm, lt::settings_pack::fixed_slots_choker);
    p.set_int(lt::settings_pack::unchoke_slots_limit, 0);
    p.set_int(lt::settings_pack::allowed_fast_set_size, 0);
    params.extensions.push_back(std::make_shared<NoUpload>());
  }
  return params;
}

// By default loopback and LAN peers join the local peer class, which ignores
// rate limits and unchoke slots. Putting every address in the global class
// makes the configured limits apply to all peers.
lt::ip_filter every_peer_global() {
  std::uint32_t const global = 1u << static_cast<std::uint32_t>(lt::session::global_peer_class_id);
  lt::ip_filter filter;
  filter.add_rule(lt::make_address("0.0.0.0"), lt::make_address("255.255.255.255"), global);
  filter.add_rule(lt::make_address("::"), lt::make_address("ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff"), global);
  return filter;
}

int metric(char const* name) {
  int const index = lt::find_metric_idx(name);
  if (index < 0) fail(FailureKind::Native, std::string("missing session metric ") + name);
  return index;
}

// asio writes a scoped IPv6 address as fe80::1%lo0, which no Rust address
// type parses, so the scope travels as its interface index.
void set_address(AlertRecord& r, lt::address const& address) {
  if (!address.is_v6()) {
    r.address = str(address.to_string());
    return;
  }
  lt::address_v6 v6 = address.to_v6();
  r.scope_id = static_cast<std::uint32_t>(v6.scope_id());
  v6.scope_id(0);
  r.address = str(v6.to_string());
}

bool torrent_record(lt::alert const* a, AlertKind kind, AlertRecord& r) {
  lt::info_hash_t const ih = static_cast<lt::torrent_alert const*>(a)->handle.info_hashes();
  // An alert can outlive its torrent. Without a live handle there is no
  // identity to report, and the removal alert carries its own.
  if (!ih.has_v1() && !ih.has_v2()) return false;
  r.kind = kind;
  r.hashes = to_hashes(ih);
  return true;
}

}

std::string describe_exception(std::exception_ptr error) {
  try {
    std::rethrow_exception(error);
  } catch (Error const& e) {
    return e.what();
  } catch (lt::system_error const& e) {
    return failure_text(FailureKind::Native, std::string(e.code().category().name()) + " " +
                                                 std::to_string(e.code().value()) + ": " +
                                                 e.code().message());
  } catch (std::exception const& e) {
    return failure_text(FailureKind::Native, e.what());
  } catch (...) {
    return failure_text(FailureKind::Native, "unknown C++ exception");
  }
}

Session::Session(SessionSettings const& settings)
    : session_(make_params(settings)),
      stats_{metric("net.sent_payload_bytes"), metric("net.recv_payload_bytes"),
             metric("net.sent_bytes"),         metric("net.recv_bytes"),
             metric("dht.dht_nodes"),          metric("peer.num_peers_connected")} {
  session_.set_peer_class_filter(every_peer_global());
}

lt::torrent_handle Session::find(rust::Slice<const std::uint8_t> id) const {
  if (id.size() != lt::sha1_hash::size()) fail(FailureKind::Native, "torrent id must be 20 bytes");
  lt::sha1_hash const key(reinterpret_cast<char const*>(id.data()));
  lt::torrent_handle h = session_.find_torrent(key);
  if (!h.is_valid()) fail(FailureKind::UnknownTorrent, "");
  return h;
}

lt::torrent_handle Session::find_with_metadata(rust::Slice<const std::uint8_t> id) const {
  lt::torrent_handle h = find(id);
  auto const ti = h.torrent_file();
  if (!ti || !ti->is_valid()) fail(FailureKind::NoMetadata, "torrent has no metadata yet");
  return h;
}

AddResult Session::add_torrent(AddParams const& p) {
  lt::add_torrent_params atp;
  switch (p.source) {
    case SourceKind::Magnet: atp = load_magnet(p.magnet); break;
    case SourceKind::Metainfo: atp = load_metainfo(as_span(p.bytes)); break;
    case SourceKind::Resume: atp = load_resume_data(as_span(p.bytes)); break;
    default: fail(FailureKind::Native, "unknown torrent source");
  }

  lt::info_hash_t const ih = atp.ti ? atp.ti->info_hashes() : atp.info_hashes;
  for (lt::torrent_handle const& existing :
       {ih.has_v1() ? session_.find_torrent(ih.v1) : lt::torrent_handle(),
        ih.has_v2() ? session_.find_torrent(ih.v2) : lt::torrent_handle()}) {
    if (existing.is_valid()) return AddResult{to_hashes(existing.info_hashes()), true};
  }

  atp.save_path = std::string(p.save_path);
  atp.flags &= ~(lt::torrent_flags::auto_managed | lt::torrent_flags::duplicate_is_error |
                 lt::torrent_flags::paused | lt::torrent_flags::default_dont_download);
  if (p.paused) atp.flags |= lt::torrent_flags::paused;

  atp.file_priorities.clear();
  if (!p.all_files) {
    atp.flags |= lt::torrent_flags::default_dont_download;
    int const known = atp.ti && atp.ti->is_valid() ? atp.ti->num_files() : -1;
    for (std::uint32_t const index : p.selected) {
      int const i = static_cast<int>(index);
      if (known >= 0 && i >= known) {
        fail(FailureKind::InvalidInput, "file index " + std::to_string(i) + " is out of range for " +
                                  std::to_string(known) + " files");
      }
      if (static_cast<int>(atp.file_priorities.size()) <= i) {
        atp.file_priorities.resize(static_cast<std::size_t>(i) + 1, lt::dont_download);
      }
      atp.file_priorities[static_cast<std::size_t>(i)] = lt::default_priority;
    }
  }

  for (auto const& tracker : p.trackers) atp.trackers.emplace_back(tracker);
  for (auto const& seed : p.web_seeds) atp.url_seeds.emplace_back(seed);
  for (auto const& peer : p.peers) atp.peers.emplace_back(parse_address(peer.address), peer.port);

  lt::torrent_handle const h = session_.add_torrent(std::move(atp));
  return AddResult{to_hashes(h.info_hashes()), false};
}

void Session::pause(rust::Slice<const std::uint8_t> id) { find(id).pause(); }

void Session::resume(rust::Slice<const std::uint8_t> id) { find(id).resume(); }

void Session::set_file_priorities(rust::Slice<const std::uint8_t> id,
                                  rust::Slice<const PriorityRecord> priorities) {
  lt::torrent_handle const h = find_with_metadata(id);
  int const files = h.torrent_file()->num_files();
  std::vector<lt::download_priority_t> all = h.get_file_priorities();
  all.resize(static_cast<std::size_t>(files), lt::default_priority);
  for (PriorityRecord const& p : priorities) {
    if (static_cast<int>(p.index) >= files) {
      fail(FailureKind::InvalidInput, "file index " + std::to_string(p.index) + " is out of range for " +
                                std::to_string(files) + " files");
    }
    all[p.index] = lt::download_priority_t(p.priority);
  }
  h.prioritize_files(all);
}

void Session::force_recheck(rust::Slice<const std::uint8_t> id) { find_with_metadata(id).force_recheck(); }

void Session::announce_to_dht(rust::Slice<const std::uint8_t> id) { find(id).force_dht_announce(); }

void Session::save_resume_data(rust::Slice<const std::uint8_t> id) {
  find(id).save_resume_data(lt::torrent_handle::flush_disk_cache | lt::torrent_handle::save_info_dict);
}

void Session::save_resume_data_if_progressed(rust::Slice<const std::uint8_t> id) {
  find(id).save_resume_data(lt::torrent_handle::flush_disk_cache | lt::torrent_handle::save_info_dict |
                            lt::torrent_handle::if_download_progress);
}

void Session::remove(rust::Slice<const std::uint8_t> id) { session_.remove_torrent(find(id)); }

void Session::connect_peer(rust::Slice<const std::uint8_t> id, rust::Str address, std::uint16_t port) {
  find(id).connect_peer(lt::tcp::endpoint(parse_address(address), port));
}

void Session::add_web_seed(rust::Slice<const std::uint8_t> id, rust::Str url) {
  find(id).add_url_seed(std::string(url));
}

rust::Vec<std::uint8_t> Session::metainfo(rust::Slice<const std::uint8_t> id) const {
  return torrent_file(id, true);
}

rust::Vec<std::uint8_t> Session::info_dictionary(rust::Slice<const std::uint8_t> id) const {
  return torrent_file(id, false);
}

rust::Vec<std::uint8_t> Session::torrent_file(rust::Slice<const std::uint8_t> id, bool piece_layers) const {
  lt::torrent_handle const h = find_with_metadata(id);
  auto const ti = h.torrent_file();
  lt::entry torrent;
  auto const info = ti->info_section();
  torrent["info"].preformatted().assign(info.begin(), info.end());
  if (piece_layers && ti->v2()) {
    lt::file_storage const& fs = ti->layout();
    auto const layers = h.piece_layers();
    auto& dict = torrent["piece layers"].dict();
    for (lt::file_index_t const f : fs.file_range()) {
      if (fs.pad_file_at(f) || fs.file_size(f) <= fs.piece_length()) continue;
      auto const i = static_cast<std::size_t>(static_cast<int>(f));
      if (i >= layers.size() || static_cast<int>(layers[i].size()) != fs.file_num_pieces(f)) {
        fail(FailureKind::NoMetadata, "piece layers are not complete yet");
      }
      std::string& layer = dict[fs.root(f).to_string()].string();
      for (lt::sha256_hash const& hash : layers[i]) layer += hash.to_string();
    }
  }
  std::vector<char> buf;
  lt::bencode(std::back_inserter(buf), torrent);
  return to_vec(buf);
}

rust::Vec<FileRecord> Session::files(rust::Slice<const std::uint8_t> id) const {
  lt::torrent_handle const h = find_with_metadata(id);
  return file_records(h.torrent_file()->layout(), h.get_renamed_files());
}

StatusRecord Session::status(rust::Slice<const std::uint8_t> id) const {
  lt::torrent_handle const h = find(id);
  lt::torrent_status const st =
      h.status(lt::torrent_handle::query_name | lt::torrent_handle::query_save_path);
  StatusRecord r{};
  r.hashes = to_hashes(st.info_hashes);
  r.name = str(st.name);
  r.save_path = str(st.save_path);
  r.state = to_state(st.state);
  r.paused = static_cast<bool>(st.flags & lt::torrent_flags::paused);
  r.error = to_error(st.errc);
  r.has_metadata = st.has_metadata;
  r.download_rate = st.download_payload_rate;
  r.upload_rate = st.upload_payload_rate;
  r.total_payload_download = st.total_payload_download;
  r.total_payload_upload = st.total_payload_upload;
  r.peers = st.num_peers;
  r.seeds = st.num_seeds;
  if (st.has_metadata) {
    auto const ti = h.torrent_file();
    lt::file_storage const& fs = ti->layout();
    std::vector<std::int64_t> verified;
    h.file_progress(verified, lt::torrent_handle::piece_granularity);
    std::vector<lt::download_priority_t> const priorities = h.get_file_priorities();
    for (lt::file_index_t const f : fs.file_range()) {
      if (fs.pad_file_at(f)) continue;
      auto const i = static_cast<std::size_t>(static_cast<int>(f));
      lt::download_priority_t const prio = i < priorities.size() ? priorities[i] : lt::default_priority;
      r.files.push_back(FileProgressRecord{static_cast<std::uint32_t>(i),
                                           static_cast<std::uint64_t>(fs.file_size(f)),
                                           static_cast<std::uint8_t>(prio),
                                           static_cast<std::uint64_t>(verified.at(i))});
    }
  }
  return r;
}

rust::Vec<PieceCounts> Session::piece_slices(rust::Slice<const std::uint8_t> id, std::uint32_t slices) const {
  lt::torrent_handle const h = find_with_metadata(id);
  lt::torrent_status const st = h.status(lt::torrent_handle::query_pieces);
  std::vector<lt::download_priority_t> const priorities = h.get_piece_priorities();
  auto const pieces = static_cast<std::size_t>(st.pieces.size());
  rust::Vec<PieceCounts> out;
  if (pieces == 0) return out;
  out.reserve(slices);
  for (std::size_t s = 0; s < slices; ++s) {
    std::size_t const begin = s * pieces / slices;
    std::size_t const end = std::max(begin + 1, (s + 1) * pieces / slices);
    PieceCounts r{};
    for (std::size_t i = begin; i < end; ++i) {
      if (i < priorities.size() && priorities[i] == lt::dont_download) continue;
      ++r.selected;
      if (st.pieces.get_bit(lt::piece_index_t(static_cast<int>(i)))) ++r.verified;
    }
    out.push_back(r);
  }
  return out;
}

MissingCounts Session::missing_pieces(rust::Slice<const std::uint8_t> id) const {
  lt::torrent_handle const h = find_with_metadata(id);
  lt::torrent_status const st = h.status(lt::torrent_handle::query_pieces);
  std::vector<lt::download_priority_t> const priorities = h.get_piece_priorities();
  std::vector<int> availability;
  h.piece_availability(availability);
  MissingCounts r{};
  for (std::size_t i = 0; i < static_cast<std::size_t>(st.pieces.size()); ++i) {
    if (i < priorities.size() && priorities[i] == lt::dont_download) continue;
    if (st.pieces.get_bit(lt::piece_index_t(static_cast<int>(i)))) continue;
    ++r.missing;
    if (i >= availability.size() || availability[i] == 0) ++r.unavailable;
  }
  return r;
}

bool Session::want_range(rust::Slice<const std::uint8_t> id, std::uint32_t file, std::uint64_t offset,
                         std::uint64_t length) {
  lt::torrent_handle const h = find_with_metadata(id);
  auto const ti = h.torrent_file();
  lt::file_storage const& fs = ti->layout();
  lt::file_index_t const f(static_cast<int>(file));
  if (file >= static_cast<std::uint32_t>(fs.num_files()) || fs.pad_file_at(f)) {
    fail(FailureKind::InvalidInput, "file index " + std::to_string(file) + " is out of range for " +
                                        std::to_string(fs.num_files()) + " files");
  }
  auto const size = static_cast<std::uint64_t>(fs.file_size(f));
  if (length == 0 || offset >= size) {
    fail(FailureKind::InvalidInput, "byte range " + std::to_string(offset) + "+" + std::to_string(length) +
                                        " is outside the " + std::to_string(size) + "-byte file");
  }
  std::uint64_t const end = length > size - offset ? size : offset + length;
  lt::piece_index_t const first = fs.map_file(f, static_cast<std::int64_t>(offset), 0).piece;
  lt::piece_index_t const last = fs.map_file(f, static_cast<std::int64_t>(end - 1), 0).piece;
  lt::piece_index_t const file_last = fs.map_file(f, static_cast<std::int64_t>(size - 1), 0).piece;
  lt::torrent_status const st = h.status(lt::torrent_handle::query_pieces);
  // A piece passes its hash check while its blocks can still wait in the
  // write cache, and leaves the download queue once they are all written.
  // Only then does reading the file return its bytes.
  std::vector<lt::partial_piece_info> const writing = h.get_download_queue();
  bool written = true;
  int deadline_ms = 0;
  for (lt::piece_index_t p = first; p <= last; ++p) {
    bool const queued = std::any_of(writing.begin(), writing.end(),
                                    [p](lt::partial_piece_info const& q) { return q.piece_index == p; });
    if (st.pieces.get_bit(p) && !queued) continue;
    written = false;
    h.set_piece_deadline(p, deadline_ms);
    deadline_ms += 100;
  }
  // A reader goes on to the bytes after the range. Sequential picking skips
  // the pieces with deadlines, so they still come first.
  h.set_sequential_range(first, file_last);
  return written;
}

void Session::download_in_order(rust::Slice<const std::uint8_t> id) {
  find(id).set_flags(lt::torrent_flags::sequential_download);
}

void Session::post_session_stats() { session_.post_session_stats(); }

rust::Vec<std::uint8_t> Session::save_state() const {
  lt::session_params const params = session_.session_state(lt::session_handle::save_dht_state);
  return to_vec(lt::write_session_params_buf(params, lt::session_handle::save_dht_state));
}

std::uint16_t Session::listen_port() const { return session_.listen_port(); }

rust::Vec<AlertRecord> Session::wait_alerts(std::uint32_t timeout_ms) {
  rust::Vec<AlertRecord> out;
  if (!session_.wait_for_alert(lt::milliseconds(timeout_ms))) return out;
  std::vector<lt::alert*> alerts;
  session_.pop_alerts(&alerts);
  for (lt::alert const* a : alerts) {
    AlertRecord r{};
    bool keep = false;
    switch (a->type()) {
      case lt::metadata_received_alert::alert_type:
        keep = torrent_record(a, AlertKind::MetadataReceived, r);
        break;
      case lt::metadata_failed_alert::alert_type:
        r.error = to_error(lt::alert_cast<lt::metadata_failed_alert>(a)->error);
        keep = torrent_record(a, AlertKind::MetadataFailed, r);
        break;
      case lt::torrent_finished_alert::alert_type:
        keep = torrent_record(a, AlertKind::TorrentFinished, r);
        break;
      case lt::torrent_paused_alert::alert_type:
        keep = torrent_record(a, AlertKind::TorrentPaused, r);
        break;
      case lt::torrent_resumed_alert::alert_type:
        keep = torrent_record(a, AlertKind::TorrentResumed, r);
        break;
      case lt::torrent_checked_alert::alert_type:
        keep = torrent_record(a, AlertKind::TorrentChecked, r);
        break;
      case lt::state_changed_alert::alert_type: {
        auto const* s = lt::alert_cast<lt::state_changed_alert>(a);
        r.from = to_state(s->prev_state);
        r.to = to_state(s->state);
        keep = torrent_record(a, AlertKind::StateChanged, r);
        break;
      }
      case lt::save_resume_data_alert::alert_type:
        r.bytes = to_vec(lt::write_resume_data_buf(lt::alert_cast<lt::save_resume_data_alert>(a)->params));
        keep = torrent_record(a, AlertKind::ResumeDataSaved, r);
        break;
      case lt::save_resume_data_failed_alert::alert_type: {
        auto const& error = lt::alert_cast<lt::save_resume_data_failed_alert>(a)->error;
        bool const unchanged = error == lt::errors::make_error_code(lt::errors::resume_data_not_modified);
        r.error = to_error(error);
        keep = torrent_record(a, unchanged ? AlertKind::ResumeDataUnchanged : AlertKind::ResumeDataFailed, r);
        break;
      }
      case lt::torrent_error_alert::alert_type: {
        auto const* e = lt::alert_cast<lt::torrent_error_alert>(a);
        r.error = to_error(e->error);
        r.path = str(e->filename());
        keep = torrent_record(a, AlertKind::TorrentError, r);
        break;
      }
      case lt::file_error_alert::alert_type: {
        auto const* e = lt::alert_cast<lt::file_error_alert>(a);
        r.error = to_error(e->error);
        r.path = str(e->filename());
        r.operation = str(lt::operation_name(e->op));
        keep = torrent_record(a, AlertKind::FileError, r);
        break;
      }
      case lt::hash_failed_alert::alert_type:
        r.piece = static_cast<int>(lt::alert_cast<lt::hash_failed_alert>(a)->piece_index);
        keep = torrent_record(a, AlertKind::HashFailed, r);
        break;
      case lt::torrent_removed_alert::alert_type:
        r.kind = AlertKind::TorrentRemoved;
        r.hashes = to_hashes(lt::alert_cast<lt::torrent_removed_alert>(a)->info_hashes);
        keep = true;
        break;
      case lt::file_prio_alert::alert_type:
        keep = torrent_record(a, AlertKind::FilePrioritiesApplied, r);
        break;
      case lt::listen_succeeded_alert::alert_type: {
        auto const* l = lt::alert_cast<lt::listen_succeeded_alert>(a);
        r.kind = AlertKind::ListenSucceeded;
        set_address(r, l->address);
        r.port = static_cast<std::uint16_t>(l->port);
        r.transport = to_transport(l->socket_type);
        keep = true;
        break;
      }
      case lt::listen_failed_alert::alert_type: {
        auto const* l = lt::alert_cast<lt::listen_failed_alert>(a);
        r.kind = AlertKind::ListenFailed;
        r.interface_name = str(l->listen_interface());
        set_address(r, l->address);
        r.port = static_cast<std::uint16_t>(l->port);
        r.transport = to_transport(l->socket_type);
        r.operation = str(lt::operation_name(l->op));
        r.error = to_error(l->error);
        keep = true;
        break;
      }
      case lt::dht_bootstrap_alert::alert_type:
        r.kind = AlertKind::DhtBootstrapped;
        keep = true;
        break;
      case lt::session_stats_alert::alert_type: {
        auto const c = lt::alert_cast<lt::session_stats_alert>(a)->counters();
        r.kind = AlertKind::SessionStats;
        r.stats = StatsRecord{c[stats_.sent_payload_bytes], c[stats_.received_payload_bytes],
                              c[stats_.sent_bytes],         c[stats_.received_bytes],
                              c[stats_.dht_nodes],          c[stats_.peers_connected]};
        keep = true;
        break;
      }
      default:
        break;
    }
    if (keep) out.push_back(std::move(r));
  }
  return out;
}

std::unique_ptr<Session> new_session(SessionSettings const& settings) {
  return std::make_unique<Session>(settings);
}

ParsedTorrent parse_magnet(rust::Str uri) { return parsed(load_magnet(uri)); }

ParsedTorrent parse_metainfo(rust::Slice<const std::uint8_t> bytes) {
  return parsed(load_metainfo(as_span(bytes)));
}

ParsedTorrent parse_resume_data(rust::Slice<const std::uint8_t> bytes) {
  return parsed(load_resume_data(as_span(bytes)));
}

rust::Vec<std::uint8_t> create_torrent(rust::Str path, std::uint32_t piece_size, bool hybrid) {
  std::string const root(path);
  lt::create_flags_t flags = lt::create_torrent::no_attributes;
  if (!hybrid) flags |= lt::create_torrent::v1_only;
  std::vector<lt::create_file_entry> files;
  try {
    files = lt::list_files(root, flags);
  } catch (lt::system_error const& e) {
    fail(FailureKind::InvalidInput, "listing " + root + ": " + e.code().message());
  }
  if (files.empty()) fail(FailureKind::InvalidInput, "no files under " + root);
  // Directory order is filesystem-dependent, and the v1 file order is part of
  // the info hash.
  std::sort(files.begin(), files.end(),
            [](lt::create_file_entry const& a, lt::create_file_entry const& b) { return a.filename < b.filename; });
  lt::create_torrent t(std::move(files), static_cast<int>(piece_size), flags);
  t.set_creator("torrential");
  lt::error_code ec;
  lt::set_piece_hashes(t, std::filesystem::path(root).parent_path().string(), ec);
  if (ec) fail(FailureKind::InvalidInput, "hashing " + root + ": " + ec.message());
  return to_vec(t.generate_buf());
}

rust::String libtorrent_version() { return rust::String(lt::version()); }

}
