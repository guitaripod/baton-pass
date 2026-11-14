#include <atomic>
#include <cstddef>
#include <cstdint>
#include <memory>
#include <pqrs/karabiner/driverkit/virtual_hid_device_driver.hpp>
#include <pqrs/karabiner/driverkit/virtual_hid_device_service.hpp>

namespace service = pqrs::karabiner::driverkit::virtual_hid_device_service;
namespace hid_report = pqrs::karabiner::driverkit::virtual_hid_device_driver::hid_report;

namespace {

enum flag : uint32_t {
  flag_connected = 1u << 0,
  flag_driver_activated = 1u << 1,
  flag_driver_connected = 1u << 2,
  flag_version_mismatched = 1u << 3,
  flag_keyboard_ready = 1u << 4,
  flag_pointing_ready = 1u << 5,
};

std::atomic<uint32_t> flags{0};
service::client* client = nullptr;

void set_flag(uint32_t bit, bool on) {
  if (on) {
    flags.fetch_or(bit);
  } else {
    flags.fetch_and(~bit);
  }
}

} // namespace

extern "C" void ferry_vhid_start(uint16_t vendor_id, uint16_t product_id, uint8_t country_code) {
  if (client) {
    return;
  }
  pqrs::dispatcher::extra::initialize_shared_dispatcher();
  client = new service::client();

  service::virtual_hid_keyboard_parameters parameters(pqrs::hid::vendor_id::value_t(vendor_id),
                                                      pqrs::hid::product_id::value_t(product_id),
                                                      pqrs::hid::country_code::value_t(country_code));

  client->connected.connect([parameters] {
    set_flag(flag_connected, true);
    client->async_virtual_hid_keyboard_initialize(parameters);
    client->async_virtual_hid_pointing_initialize();
  });
  client->connect_failed.connect([](auto&&) {
    set_flag(flag_connected, false);
  });
  client->closed.connect([] {
    flags.store(0);
  });
  client->driver_activated.connect([](bool value) {
    set_flag(flag_driver_activated, value);
  });
  client->driver_connected.connect([](bool value) {
    set_flag(flag_driver_connected, value);
  });
  client->driver_version_mismatched.connect([](bool value) {
    set_flag(flag_version_mismatched, value);
  });
  client->virtual_hid_keyboard_ready.connect([](bool value) {
    set_flag(flag_keyboard_ready, value);
  });
  client->virtual_hid_pointing_ready.connect([](bool value) {
    set_flag(flag_pointing_ready, value);
  });
  client->async_start();
}

extern "C" uint32_t ferry_vhid_flags(void) {
  return flags.load();
}

extern "C" void ferry_vhid_keyboard(uint8_t modifiers, const uint16_t* keys, size_t count) {
  if (!client) {
    return;
  }
  hid_report::keyboard_input report;
  for (int bit = 0; bit < 8; ++bit) {
    if (modifiers & (1u << bit)) {
      report.modifiers.insert(static_cast<hid_report::modifier>(1u << bit));
    }
  }
  for (size_t index = 0; index < count; ++index) {
    report.keys.insert(keys[index]);
  }
  client->async_post_report(report);
}

extern "C" void ferry_vhid_consumer(const uint16_t* keys, size_t count) {
  if (!client) {
    return;
  }
  hid_report::consumer_input report;
  for (size_t index = 0; index < count; ++index) {
    report.keys.insert(keys[index]);
  }
  client->async_post_report(report);
}

extern "C" void ferry_vhid_pointing(uint32_t buttons, int8_t x, int8_t y, int8_t vertical_wheel, int8_t horizontal_wheel) {
  if (!client) {
    return;
  }
  hid_report::pointing_input report;
  for (uint8_t button = 1; button <= 32; ++button) {
    if (buttons & (1u << (button - 1))) {
      report.buttons.insert(button);
    }
  }
  report.x = static_cast<uint8_t>(x);
  report.y = static_cast<uint8_t>(y);
  report.vertical_wheel = static_cast<uint8_t>(vertical_wheel);
  report.horizontal_wheel = static_cast<uint8_t>(horizontal_wheel);
  client->async_post_report(report);
}
