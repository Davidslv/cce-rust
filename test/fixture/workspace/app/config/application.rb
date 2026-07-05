module App
  # Presence of Gemfile + config/application.rb marks this member as a rails-app
  # (detection rule 2). Kept minimal and neutral on purpose.
  class Application
    def boot
      "app booted"
    end
  end
end
