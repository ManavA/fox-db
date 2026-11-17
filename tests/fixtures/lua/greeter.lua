local Greeter = {}

function format_name(s)
  return "Hello " .. s
end

function Greeter.greet(who)
  return format_name(who)
end

function main()
  Greeter.greet("x")
end
